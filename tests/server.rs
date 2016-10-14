#![deny(warnings)]
extern crate hyper;
extern crate futures;
extern crate spmc;

use futures::Future;
use futures::stream::Stream;

use std::net::{TcpStream, SocketAddr};
use std::io::{Read, Write};
use std::sync::mpsc;
use std::time::Duration;

use hyper::server::{Server, Request, Response, Service};

struct Serve {
    listening: Option<hyper::server::Listening>,
    msg_rx: mpsc::Receiver<Msg>,
    reply_tx: spmc::Sender<Reply>,
    spawn_rx: mpsc::Receiver<()>,
}

impl Serve {
    fn addr(&self) -> &SocketAddr {
        self.listening.as_ref().unwrap().addr()
    }

    /*
    fn head(&self) -> Request {
        unimplemented!()
    }
    */

    fn body(&self) -> Vec<u8> {
        let mut buf = vec![];
        while let Ok(Msg::Chunk(msg)) = self.msg_rx.try_recv() {
            buf.extend(&msg);
        }
        buf
    }

    fn reply(&self) -> ReplyBuilder {
        ReplyBuilder {
            tx: &self.reply_tx
        }
    }
}

struct ReplyBuilder<'a> {
    tx: &'a spmc::Sender<Reply>,
}

impl<'a> ReplyBuilder<'a> {
    fn status(self, status: hyper::StatusCode) -> Self {
        self.tx.send(Reply::Status(status)).unwrap();
        self
    }

    fn header<H: hyper::header::Header>(self, header: H) -> Self {
        let mut headers = hyper::Headers::new();
        headers.set(header);
        self.tx.send(Reply::Headers(headers)).unwrap();
        self
    }

    fn body<T: AsRef<[u8]>>(self, body: T) {
        self.tx.send(Reply::Body(body.as_ref().into())).unwrap();
    }
}

impl Drop for Serve {
    fn drop(&mut self) {
        self.listening.take().unwrap().close();
        self.spawn_rx.recv().expect("server thread should shutdown cleanly");
    }
}

#[derive(Clone)]
struct TestService {
    tx: mpsc::Sender<Msg>,
    reply: spmc::Receiver<Reply>,
    _timeout: Option<Duration>,
}

#[derive(Clone, Debug)]
enum Reply {
    Status(hyper::StatusCode),
    Headers(hyper::Headers),
    Body(Vec<u8>),
}

enum Msg {
    //Head(Request),
    Chunk(Vec<u8>),
}

impl Service for TestService {
    type Request = Request;
    type Response = Response;
    type Error = hyper::Error;
    type Future = Box<Future<Item=Response, Error=hyper::Error>>;
    fn call(&self, req: Request) -> Self::Future {
        let tx = self.tx.clone();
        let replies = self.reply.clone();
        req.body().for_each(move |chunk| {
            tx.send(Msg::Chunk(chunk)).unwrap();
            Ok(())
        }).and_then(move |_| {
            let mut res = Response::new();
            while let Ok(reply) = replies.try_recv() {
                match reply {
                    Reply::Status(s) => {
                        res = res.status(s);
                    },
                    Reply::Headers(headers) => {
                        res = res.headers(headers);
                    },
                    Reply::Body(body) => {
                        res = res.body(body);
                    },
                }
            }
            ::futures::finished(res)
        }).boxed()
    }

    fn poll_ready(&self) -> ::futures::Async<()> {
        ::futures::Async::Ready(())
    }
}

fn connect(addr: &SocketAddr) -> TcpStream {
    let req = TcpStream::connect(addr).unwrap();
    req.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    req.set_write_timeout(Some(Duration::from_secs(5))).unwrap();
    req
}

fn serve() -> Serve {
    serve_with_timeout(None)
}

fn serve_with_timeout(dur: Option<Duration>) -> Serve {
    use std::thread;

    let (thread_tx, thread_rx) = mpsc::channel();
    let (spawn_tx, spawn_rx) = mpsc::channel();
    let (msg_tx, msg_rx) = mpsc::channel();
    let (reply_tx, reply_rx) = spmc::channel();

    let addr = "127.0.0.1:0".parse().unwrap();

    let thread_name = format!("test-server-{:?}", dur);
    thread::Builder::new().name(thread_name).spawn(move || {
        let (listening, server) = Server::http(&addr).unwrap()
            .handle(TestService {
                tx: msg_tx.clone(),
                _timeout: dur,
                reply: reply_rx,
            }).unwrap();
        thread_tx.send(listening).unwrap();
        server.run();
        spawn_tx.send(()).unwrap();
    }).unwrap();

    let listening = thread_rx.recv().unwrap();

    Serve {
        listening: Some(listening),
        msg_rx: msg_rx,
        reply_tx: reply_tx,
        spawn_rx: spawn_rx,
    }
}

#[test]
fn server_get_should_ignore_body() {
    let server = serve();

    let mut req = connect(server.addr());
    // Connection: close = don't try to parse the body as a new request
    req.write_all(b"\
        GET / HTTP/1.1\r\n\
        Host: example.domain\r\n\
        Connection: close\r\n
        \r\n\
        I shouldn't be read.\r\n\
    ").unwrap();
    req.read(&mut [0; 256]).unwrap();

    assert_eq!(server.body(), b"");
}

#[test]
fn server_get_with_body() {
    let server = serve();
    let mut req = connect(server.addr());
    req.write_all(b"\
        GET / HTTP/1.1\r\n\
        Host: example.domain\r\n\
        Content-Length: 19\r\n\
        \r\n\
        I'm a good request.\r\n\
    ").unwrap();
    req.read(&mut [0; 256]).unwrap();

    // note: doesnt include trailing \r\n, cause Content-Length wasn't 21
    assert_eq!(server.body(), b"I'm a good request.");
}

#[test]
fn server_get_fixed_response() {
    extern crate pretty_env_logger;
    pretty_env_logger::init();
    let foo_bar = b"foo bar baz";
    let server = serve();
    server.reply()
        .status(hyper::Ok)
        .header(hyper::header::ContentLength(foo_bar.len() as u64))
        .body(foo_bar);
    let mut req = connect(server.addr());
    req.write_all(b"\
        GET / HTTP/1.1\r\n\
        Host: example.domain\r\n\
        Connection: close\r\n\
        \r\n\
    ").unwrap();
    let mut body = String::new();
    req.read_to_string(&mut body).unwrap();
    let n = body.find("\r\n\r\n").unwrap() + 4;

    assert_eq!(&body[n..], "foo bar baz");
}

#[test]
fn server_get_chunked_response() {
    let foo_bar = b"foo bar baz";
    let server = serve();
    server.reply()
        .status(hyper::Ok)
        .header(hyper::header::TransferEncoding::chunked())
        .body(foo_bar);
    let mut req = connect(server.addr());
    req.write_all(b"\
        GET / HTTP/1.1\r\n\
        Host: example.domain\r\n\
        Connection: close\r\n
        \r\n\
    ").unwrap();
    let mut body = String::new();
    req.read_to_string(&mut body).unwrap();
    let n = body.find("\r\n\r\n").unwrap() + 4;

    assert_eq!(&body[n..], "B\r\nfoo bar baz\r\n0\r\n\r\n");
}

#[test]
fn server_post_with_chunked_body() {
    let server = serve();
    let mut req = connect(server.addr());
    req.write_all(b"\
        POST / HTTP/1.1\r\n\
        Host: example.domain\r\n\
        Transfer-Encoding: chunked\r\n\
        \r\n\
        1\r\n\
        q\r\n\
        2\r\n\
        we\r\n\
        2\r\n\
        rt\r\n\
        0\r\n\
        \r\n\
    ").unwrap();
    req.read(&mut [0; 256]).unwrap();

    assert_eq!(server.body(), b"qwert");
}

#[test]
fn server_empty_response_chunked() {
    let server = serve();

    server.reply()
        .status(hyper::Ok)
        .body("");

    let mut req = connect(server.addr());
    req.write_all(b"\
        GET / HTTP/1.1\r\n\
        Host: example.domain\r\n\
        Content-Length: 0\r\n
        Connection: close\r\n
        \r\n\
    ").unwrap();


    let mut response = String::new();
    req.read_to_string(&mut response).unwrap();

    assert!(response.contains("Transfer-Encoding: chunked\r\n"));

    let mut lines = response.lines();
    assert_eq!(lines.next(), Some("HTTP/1.1 200 OK"));

    let mut lines = lines.skip_while(|line| !line.is_empty());
    assert_eq!(lines.next(), Some(""));
    // 0\r\n\r\n
    assert_eq!(lines.next(), Some("0"));
    assert_eq!(lines.next(), Some(""));
    assert_eq!(lines.next(), None);
}

#[test]
fn server_empty_response_chunked_without_calling_write() {
    let server = serve();
    server.reply()
        .status(hyper::Ok)
        .header(hyper::header::TransferEncoding::chunked());
    let mut req = connect(server.addr());
    req.write_all(b"\
        GET / HTTP/1.1\r\n\
        Host: example.domain\r\n\
        Connection: close\r\n
        \r\n\
    ").unwrap();

    let mut response = String::new();
    req.read_to_string(&mut response).unwrap();

    assert!(response.contains("Transfer-Encoding: chunked\r\n"));

    let mut lines = response.lines();
    assert_eq!(lines.next(), Some("HTTP/1.1 200 OK"));

    let mut lines = lines.skip_while(|line| !line.is_empty());
    assert_eq!(lines.next(), Some(""));
    // 0\r\n\r\n
    assert_eq!(lines.next(), Some("0"));
    assert_eq!(lines.next(), Some(""));
    assert_eq!(lines.next(), None);
}

#[test]
fn server_keep_alive() {
    let foo_bar = b"foo bar baz";
    let server = serve();
    server.reply()
        .status(hyper::Ok)
        .header(hyper::header::ContentLength(foo_bar.len() as u64))
        .body(foo_bar);
    let mut req = connect(server.addr());
    req.write_all(b"\
        GET / HTTP/1.1\r\n\
        Host: example.domain\r\n\
        Connection: keep-alive\r\n\
        \r\n\
    ").expect("writing 1");

    let mut buf = [0; 1024 * 8];
    loop {
        let n = req.read(&mut buf[..]).expect("reading 1");
        if n < buf.len() {
            if &buf[n - foo_bar.len()..n] == foo_bar {
                break;
            } else {
                println!("{:?}", ::std::str::from_utf8(&buf[..n]));
            }
        }
    }

    // try again!

    let quux = b"zar quux";
    server.reply()
        .status(hyper::Ok)
        .header(hyper::header::ContentLength(quux.len() as u64))
        .body(quux);
    req.write_all(b"\
        GET /quux HTTP/1.1\r\n\
        Host: example.domain\r\n\
        Connection: close\r\n\
        \r\n\
    ").expect("writing 2");

    let mut buf = [0; 1024 * 8];
    loop {
        let n = req.read(&mut buf[..]).expect("reading 2");
        assert!(n > 0, "n = {}", n);
        if n < buf.len() {
            if &buf[n - quux.len()..n] == quux {
                break;
            }
        }
    }
}

/*
#[test]
fn server_get_with_body_three_listeners() {
    let server = serve_n(3);
    let addrs = server.addrs();
    assert_eq!(addrs.len(), 3);

    for (i, addr) in addrs.iter().enumerate() {
        let mut req = TcpStream::connect(addr).unwrap();
        write!(req, "\
            GET / HTTP/1.1\r\n\
            Host: example.domain\r\n\
            Content-Length: 17\r\n\
            \r\n\
            I'm sending to {}.\r\n\
        ", i).unwrap();
        req.read(&mut [0; 256]).unwrap();

        // note: doesnt include trailing \r\n, cause Content-Length wasn't 19
        let comparison = format!("I'm sending to {}.", i).into_bytes();
        assert_eq!(server.body(), comparison);
    }
}
*/

use std::borrow::Cow;
use std::fmt;
use std::hash::Hash;
use std::io::{self, Write};
use std::marker::PhantomData;
use std::mem;
use std::time::Duration;

use futures::{Poll, Async};
use tokio::io::{Io, FramedIo};
use tokio_proto::pipeline::Frame;

use http::{self, h1, Http1Transaction, IoBuf, WriteBuf};
use http::h1::{Encoder, Decoder};
use http::buffer::Buffer;
use version::HttpVersion;


/// This handles a connection, which will have been established over a
/// Transport (like a socket), and will likely include multiple
/// `Transaction`s over HTTP.
///
/// The connection will determine when a message begins and ends, creating
/// a new message `TransactionHandler` for each one, as well as determine if this
/// connection can be kept alive after the message, or if it is complete.
pub struct Conn<I, T> {
    io: IoBuf<I>,
    keep_alive_enabled: bool,
    state: State,
    _marker: PhantomData<T>
}

impl<I, T> Conn<I, T> {
    pub fn new(transport: I) -> Conn<I, T> {
        Conn {
            io: IoBuf {
                read_buf: Buffer::new(),
                write_buf: Buffer::new(),
                transport: transport,
            },
            keep_alive_enabled: true,
            state: State {
                reading: Reading::Init,
                writing: Writing::Init,
            },
            _marker: PhantomData,
        }
    }
}

impl<I: Io, T: Http1Transaction> Conn<I, T> {

    fn parse(&mut self) -> ::Result<Option<http::MessageHead<T::Incoming>>> {
        self.io.parse::<T>()
    }

    fn is_closed(&self) -> bool {
        self.state.is_closed()
    }

    fn can_read_head(&self) -> bool {
        match self.state.reading {
            Reading::Init => true,
            _ => false,
        }
    }

    fn read_head(&mut self) -> Poll<Frame<http::MessageHead<T::Incoming>, http::Chunk, ::Error>, io::Error> {
        debug_assert!(self.can_read_head());
        trace!("Conn::read_head");

        let (version, head) = match self.parse() {
            Ok(Some(head)) => (head.version, head),
            Ok(None) => return Ok(Async::NotReady),
            Err(e) => {
                self.state.close();
                self.io.read_buf.consume_leading_lines();
                if !self.io.read_buf.is_empty() {
                    error!("parse error ({}) with bytes: {:?}", e, self.io.read_buf.bytes());
                    return Ok(Async::Ready(Frame::Error { error: e }));
                } else {
                    trace!("parse error with 0 input, err = {:?}", e);
                    return Ok(Async::Ready(Frame::Done));
                }
            }
        };

        match version {
            HttpVersion::Http10 | HttpVersion::Http11 => {
                let decoder = match T::decoder(&head) {
                    Ok(d) => d,
                    Err(e) => {
                        error!("decoder error = {:?}", e);
                        self.state.close();
                        return Ok(Async::Ready(Frame::Error { error: e }));
                    }
                };
                let (body, reading) = if decoder.is_eof() {
                    (false, Reading::KeepAlive)
                } else {
                    (true, Reading::Body(decoder))
                };
                self.state = State {
                    reading: reading,
                    writing: Writing::Init,
                };
                return Ok(Async::Ready(Frame::Message { message: head, body: body }));
            },
            _ => {
                error!("unimplemented HTTP Version = {:?}", version);
                self.state.close();
                return Ok(Async::Ready(Frame::Error { error: ::Error::Version }));
            }
        }
    }

    fn read_body(&mut self) -> Poll<Option<http::Chunk>, io::Error> {
        debug_assert!(!self.can_read_head());

        trace!("Conn::read_body");

        match self.state.reading {
            Reading::Body(ref mut decoder) => {
                //TODO use an appendbuf or something
                let mut buf = vec![0; 1024 * 4];
                let n = try!(decoder.decode(&mut self.io, &mut buf));
                if n > 0 {
                    buf.truncate(n);
                    Ok(Async::Ready(Some(buf)))
                } else {
                    Ok(Async::Ready(None))
                }

            },
            _ => unimplemented!("Reading::*")
        }
    }
}

impl<I, T> FramedIo for Conn<I, T>
where I: Io,
      T: Http1Transaction,
      T::Outgoing: fmt::Debug {
    type In = Frame<http::MessageHead<T::Outgoing>, http::Chunk, ::Error>;
    type Out = Frame<http::MessageHead<T::Incoming>, http::Chunk, ::Error>;

    fn poll_read(&mut self) -> Async<()> {
        let ret = match self.state.reading {
            Reading::Closed => Async::Ready(()),
            _ => self.io.transport.poll_read()
        };
        trace!("Conn::poll_read = {:?}", ret);
        ret
    }

    fn read(&mut self) -> Poll<Self::Out, io::Error> {
        trace!("Conn::read");

        if self.is_closed() {
            trace!("Conn::read when closed");
            return Ok(Async::Ready(Frame::Done));
        }

        if self.can_read_head() {
            self.read_head()
        } else {
            self.read_body().map(|async| async.map(|chunk| Frame::Body { chunk: chunk }))
        }
    }

    fn poll_write(&mut self) -> Async<()> {
        trace!("Conn::poll_write");
        //self.io.transport.poll_write()
        Async::Ready(())
    }

    fn write(&mut self, frame: Self::In) -> Poll<(), io::Error> {
        trace!("Conn::write");
        match self.state.writing {
            Writing::Init => {
                match frame {
                    Frame::Message { message: mut head, body } => {
                        trace!("Conn::write Frame::Message with_body = {:?}", body);
                        let mut buf = Vec::new();
                        T::encode(&mut head, &mut buf);
                        self.io.write(&buf).unwrap();
                        self.state = State {
                            writing: Writing::Init,
                            reading: Reading::Init,
                        };
                    },
                    Frame::Error { error } => {
                        error!("Conn::write Frame::Error err = {:?}", error);
                        self.state.close();
                    },
                    Frame::Done => {
                        trace!("Conn::write Frame::Done");
                        self.state.close();
                    }
                    other => {
                        trace!("writing illegal frame at State:Init: {:?}", other);
                        self.state.close();
                        return Err(io::Error::new(io::ErrorKind::InvalidInput, "illegal frame"));
                    }
                }
                return Ok(Async::Ready(()));
            },
            Writing::Body(_) => {
                match frame {
                    Frame::Body { chunk: Some(body) } => {
                        trace!("Conn::write Http1 Frame::Body = Some");
                        self.io.write(&body).unwrap();
                    },
                    Frame::Body { chunk: None } => {
                        trace!("Conn::write Http1 Frame::Body = None");
                    },
                    Frame::Message { .. } => {
                        error!("received Message frame when expecting Body: {:?}", frame);
                        return Err(io::Error::new(io::ErrorKind::InvalidInput, "received Message when expecting Body"));
                    },
                    Frame::Error { error } => {
                        error!("Conn::write Frame::Error err = {:?}", error);
                        self.state = State {
                            reading: Reading::Closed,
                            writing: Writing::Closed,
                        };
                    },
                    Frame::Done => {
                        trace!("Conn::write Frame::Done");
                        self.state = State {
                            reading: Reading::Closed,
                            writing: Writing::Closed,
                        };
                    }
                }
                return Ok(Async::Ready(()));
            }
            Writing::KeepAlive | Writing::Closed => {
                error!("Conn::write Closed frame = {:?}", frame);
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "write when closed"));
            }
        }
    }

    fn flush(&mut self) -> Poll<(), io::Error> {
        trace!("Conn::flush");
        match self.io.flush() {
            Ok(()) => {
                if self.poll_read().is_ready() {
                    ::futures::task::park().unpark();
                }
                Ok(Async::Ready(()))
            },
            Err(e) => match e.kind() {
                io::ErrorKind::WouldBlock => Ok(Async::NotReady),
                _ => Err(e)
            }
        }
    }
}

impl<I, T> fmt::Debug for Conn<I, T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Conn")
            .field("keep_alive_enabled", &self.keep_alive_enabled)
            .field("state", &self.state)
            .field("io", &self.io)
            .finish()
    }
}

#[derive(Debug)]
struct State {
    reading: Reading,
    writing: Writing,
}

#[derive(Debug)]
enum Reading {
    Init,
    Body(Decoder),
    KeepAlive,
    Closed,
}

#[derive(Debug)]
enum Writing {
    Init,
    Body(Encoder),
    KeepAlive,
    Closed,
}

impl State {
    fn close(&mut self) {
        self.reading = Reading::Closed;
        self.writing = Writing::Closed;
    }

    fn is_closed(&self) -> bool {
        match (&self.reading, &self.writing) {
            (&Reading::Closed, &Writing::Closed) => true,
            _ => false
        }
    }
}

#[cfg(test)]
mod tests {
    use futures::Async;
    use tokio::io::FramedIo;
    use tokio_proto::pipeline::Frame;

    use http::{MessageHead, ServerTransaction};
    use mock::AsyncIo;

    use super::{Conn, State};

    #[test]
    fn test_conn_init_read() {
        let good_message = b"GET / HTTP/1.1\r\n\r\n".to_vec();
        let len = good_message.len();
        let io = AsyncIo::new_buf(good_message, len);
        let mut conn = Conn::<_, ServerTransaction>::new(io);

        match conn.read().unwrap() {
            Async::Ready(Frame::Message { message, body: false }) => {
                assert_eq!(message, MessageHead {
                    subject: ::http::RequestLine(::Get, ::RequestUri::AbsolutePath {
                        path: "/".to_string(),
                        query: None,
                    }),
                    .. MessageHead::default()
                })
            },
            f => panic!("frame is not Frame::Message: {:?}", f)
        }
    }

    #[test]
    fn test_conn_closed_read() {
        let io = AsyncIo::new_buf(vec![], 0);
        let mut conn = Conn::<_, ServerTransaction>::new(io);
        conn.state.close();

        match conn.read().unwrap() {
            Async::Ready(Frame::Done) => {},
            other => panic!("frame is not Frame::Done: {:?}", other)
        }
    }

    // test_conn_init_write

    #[test]
    fn test_conn_closed_write() {
        let io = AsyncIo::new_buf(vec![], 0);
        let mut conn = Conn::<_, ServerTransaction>::new(io);
        conn.state.close();

        match conn.write(Frame::Body { chunk: Some(b"foobar".to_vec()) }) {
            Err(e) => {},
            other => panic!("did not return Err: {:?}", other)
        }

        assert!(conn.state.is_closed());
    }
}

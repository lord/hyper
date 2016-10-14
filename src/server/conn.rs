use std::io;

use futures::{Async, Poll};
use tokio_proto::pipeline::Frame;
use tokio::io::{Io, FramedIo};

use http;
use server::{Request, Response};

pub struct Conn<I> {
    inner: http::Conn<I, http::ServerTransaction>,
}

impl<I> Conn<I> {
    pub fn new(io: I) -> Conn<I> {
        Conn {
            inner: http::Conn::new(io)
        }
    }
}

impl<I> FramedIo for Conn<I> where I: Io {
    type In = Frame<Response, Vec<u8>, ::Error>;
    type Out = Frame<Request, Vec<u8>, ::Error>;

    fn poll_read(&mut self) -> Async<()> {
        self.inner.poll_read()
    }

    fn read(&mut self) -> Poll<Self::Out, io::Error> {
        self.inner.read().map(|async| match async {
            Async::Ready(Frame::Message { message, _ )) => {
                Async::Ready(Frame::Message {
                    message: Request::new(message),
                    body: false,
                })
            },
            Async::NotReady => Async::NotReady,
            a => unimplemented!("Conn::read Frame::*")
        })
    }

    fn poll_write(&mut self) -> Async<()> {
        self.inner.poll_write()
    }

    fn write(&mut self, frame: Self::In) -> Poll<(), io::Error> {
        match frame {
            Frame::Message(response) => {
                self.inner.write(Frame::Message(response.head))
            },
            _ => unimplemented!("Conn::write Frame::*")
        }
    }

    fn flush(&mut self) -> Poll<(), io::Error> {
        self.inner.flush()
    }
}

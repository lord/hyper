//! Server Requests
//!
//! These are requests that a `hyper::Server` receives, and include its method,
//! target URI, headers, and message body.

use std::fmt;

use futures::stream::Receiver;

use version::HttpVersion;
use method::Method;
use header::Headers;
use http::{RequestHead, MessageHead, RequestLine, Chunk};
use uri::RequestUri;


/// A request bundles several parts of an incoming `NetworkStream`, given to a `Handler`.
pub struct Request {
    method: Method,
    uri: RequestUri,
    version: HttpVersion,
    headers: Headers,
    body: Option<Receiver<Chunk, ::Error>>,
}

impl Request {
    /// Construct a new Request.
    #[inline]
    pub fn new(incoming: RequestHead) -> Request {
        let MessageHead { version, subject: RequestLine(method, uri), headers } = incoming;
        debug!("Request Line: {:?} {:?} {:?}", method, uri, version);
        debug!("{:#?}", headers);

        Request {
            method: method,
            uri: uri,
            headers: headers,
            version: version,
            body: None,
        }
    }

    /// The `Method`, such as `Get`, `Post`, etc.
    #[inline]
    pub fn method(&self) -> &Method { &self.method }

    /// The headers of the incoming request.
    #[inline]
    pub fn headers(&self) -> &Headers { &self.headers }

    /// The target request-uri for this request.
    #[inline]
    pub fn uri(&self) -> &RequestUri { &self.uri }

    /// The version of HTTP for this request.
    #[inline]
    pub fn version(&self) -> &HttpVersion { &self.version }

    /// The target path of this Request.
    #[inline]
    pub fn path(&self) -> Option<&str> {
        match self.uri {
            RequestUri::AbsolutePath { path: ref p, .. } => Some(p.as_str()),
            RequestUri::AbsoluteUri(ref url) => Some(url.path()),
            _ => None,
        }
    }

    /// The query string of this Request.
    #[inline]
    pub fn query(&self) -> Option<&str> {
        match self.uri {
            RequestUri::AbsolutePath { query: ref q, .. } => q.as_ref().map(|x| x.as_str()),
            RequestUri::AbsoluteUri(ref url) => url.query(),
            _ => None,
        }
    }

    #[inline]
    pub fn body(self) -> Receiver<::http::Chunk, ::Error> {
        self.body.unwrap_or_else(|| {
            let (tx, rx) = ::futures::stream::channel();
            rx
        })
    }

    /// Deconstruct this Request into its pieces.
    ///
    /// Modifying these pieces will have no effect on how hyper behaves.
    #[inline]
    pub fn deconstruct(self) -> (Method, RequestUri, HttpVersion, Headers) {
        (self.method, self.uri, self.version, self.headers)
    }

}

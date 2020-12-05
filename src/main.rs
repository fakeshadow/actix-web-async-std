#![feature(type_alias_impl_trait)]

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use actix_server::{FromMio, MioStream, ServiceStream};
use actix_web::dev::{Service, Transform};
use actix_web::rt::{RuntimeFactory, RuntimeService, SleepService};
use actix_web::{get, App, HttpServer};
use async_std::io::{Read, Write};
use openssl::ssl::{SslAcceptor, SslFiletype, SslMethod};
use tokio::io::{AsyncRead, AsyncWrite};

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    // load ssl keys
    let mut builder = SslAcceptor::mozilla_intermediate(SslMethod::tls()).unwrap();
    builder
        .set_private_key_file("src/key.pem", SslFiletype::PEM)
        .unwrap();
    builder.set_certificate_chain_file("src/cert.pem").unwrap();

    HttpServer::new_with::<AsyncStdRtFactory>(|| App::new().wrap(TestMiddleware).service(index))
        .bind_with::<AsyncStdTcpStream, _>("0.0.0.0:8080")?
        .bind_openssl_with::<AsyncStdTcpStream, _>("0.0.0.0:8081", builder)?
        .run()
        .await
}

#[get("/")]
async fn index() -> &'static str {
    "Ok"
}

struct TestMiddleware;

impl<S> Transform<S> for TestMiddleware
where
    S: Service,
{
    type Request = S::Request;
    type Response = S::Response;
    type Error = S::Error;
    type Transform = TestMiddlewareService<S>;
    type InitError = ();
    type Future = impl Future<Output = Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        async move {
            async_io::Timer::after(Duration::from_micros(1)).await;
            Ok(TestMiddlewareService { service })
        }
    }
}

struct TestMiddlewareService<S> {
    service: S,
}

impl<S> Service for TestMiddlewareService<S>
where
    S: Service,
{
    type Request = S::Request;
    type Response = S::Response;
    type Error = S::Error;
    type Future = impl Future<Output = Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.service.poll_ready(cx)
    }

    fn call(&mut self, req: Self::Request) -> Self::Future {
        let fut = self.service.call(req);
        async move {
            println!("passing through");
            fut.await
        }
    }
}

// custom runtime factory
struct AsyncStdRtFactory;

// custom runtime instance
struct AsyncStdRt;

// custom runtime sleep timer
struct AsyncStdSleep {
    timer: async_io::Timer,
    deadline: Instant,
}

impl Future for AsyncStdSleep {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.get_mut().timer).poll(cx).map(|_| ())
    }
}

// stream type can work on the custom runtime
struct AsyncStdTcpStream(async_std::net::TcpStream);

// impl traits for custom runtime
impl RuntimeFactory for AsyncStdRtFactory {
    type Runtime = AsyncStdRt;

    fn create() -> std::io::Result<Self::Runtime> {
        Ok(AsyncStdRt)
    }

    fn block_on<F: Future>(_: &mut Self::Runtime, f: F) -> <F as Future>::Output {
        async_std::task::block_on(f)
    }

    fn spawn<F: Future<Output = ()> + 'static>(_: &mut Self::Runtime, f: F) {
        async_std::task::spawn_local(f);
    }
}

// impl runtime methods for custom runtime
impl RuntimeService for AsyncStdRt {
    type Sleep = AsyncStdSleep;

    fn spawn<F: Future<Output = ()> + 'static>(f: F) {
        async_std::task::spawn_local(f);
    }

    fn sleep(dur: Duration) -> Self::Sleep {
        let dead_line = Instant::now() + dur;
        let timer = async_io::Timer::at(dead_line);
        AsyncStdSleep {
            deadline: dead_line,
            timer,
        }
    }

    fn sleep_until(dead_line: Instant) -> Self::Sleep {
        let timer = async_io::Timer::at(dead_line);
        AsyncStdSleep {
            deadline: dead_line,
            timer,
        }
    }
}

// necessary trait for internal methods that would be called from stream type.
impl ServiceStream for AsyncStdTcpStream {
    type Runtime = AsyncStdRt;

    fn peer_addr(&self) -> Option<SocketAddr> {
        self.0.peer_addr().ok()
    }
}

// actix-web use tokio::io::{AsyncRead, AsyncWrite} traits handle stream. so your stream must be
// able to impl these traits.
impl AsyncRead for AsyncStdTcpStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().0).poll_read(cx, buf)
    }
}

impl AsyncWrite for AsyncStdTcpStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        Pin::new(&mut self.get_mut().0).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        Pin::new(&mut self.get_mut().0).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        Pin::new(&mut self.get_mut().0).poll_close(cx)
    }
}

// server would dispatch mio stream. use this trait to convert to the stream type we want on target
// runtime
impl FromMio for AsyncStdTcpStream {
    fn from_mio(sock: MioStream) -> std::io::Result<Self> {
        match sock {
            MioStream::Tcp(mio) => {
                // change to IntoRawSocket/FromRowSocket on windows target.
                let raw = std::os::unix::prelude::IntoRawFd::into_raw_fd(mio);
                let stream = unsafe { std::os::unix::prelude::FromRawFd::from_raw_fd(raw) };
                Ok(AsyncStdTcpStream(stream))
            }
            MioStream::Uds(_) => {
                panic!("Should not happen, bug in server impl");
            }
        }
    }
}

// methods used to check deadline and reset sleep timer.
impl SleepService for AsyncStdSleep {
    fn sleep_deadline(&self) -> Instant {
        self.deadline
    }

    fn sleep_reset(&mut self, deadline: Instant) {
        self.timer.set_at(deadline);
        self.deadline = deadline;
    }
}

#[macro_use]
extern crate unwrap;
#[macro_use]
extern crate net_literals;
extern crate tokio_core;
extern crate p2p;
extern crate futures;

use futures::{Future, Stream};
use p2p::TcpListenerExt;
use tokio_core::net::TcpListener;

fn main() {
    let mut core = unwrap!(tokio_core::reactor::Core::new());
    let handle = core.handle();
    let res = core.run({
        TcpListener::bind_public(&addr!("0.0.0.0:0"), &handle)
            .map_err(|e| panic!("Error binding listener publicly: {}", e))
            .and_then(|(listener, public_addr)| {
                println!("listening on public socket address {}", public_addr);
                listener.incoming().for_each(|(_stream, addr)| {
                    println!("got connection from {}", addr);
                    Ok(())
                })
            })
    });
    unwrap!(res);
}

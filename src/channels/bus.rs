//! A tiny typed event bus: any number of publishers and subscribers, no coupling
//! between them. Channels publish inbound events; the gateway (and anything else,
//! e.g. a logger) subscribes.

use tokio::sync::broadcast;

pub struct Bus<T: Clone> {
    tx: broadcast::Sender<T>,
}

impl<T: Clone> Clone for Bus<T> {
    fn clone(&self) -> Self {
        Self { tx: self.tx.clone() }
    }
}

impl<T: Clone> Bus<T> {
    pub fn new(capacity: usize) -> Self {
        Self {
            tx: broadcast::channel(capacity).0,
        }
    }

    /// Delivers `event` to every current subscriber; no subscribers is not an error.
    pub fn publish(&self, event: T) {
        let _ = self.tx.send(event);
    }

    pub fn subscribe(&self) -> Subscription<T> {
        Subscription {
            rx: self.tx.subscribe(),
        }
    }
}

pub struct Subscription<T: Clone> {
    rx: broadcast::Receiver<T>,
}

impl<T: Clone> Subscription<T> {
    /// Next event, or `None` once every publisher is gone. A slow subscriber skips
    /// the events it missed (and says so on stderr) instead of failing.
    pub async fn recv(&mut self) -> Option<T> {
        loop {
            match self.rx.recv().await {
                Ok(e) => return Some(e),
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    eprintln!("bus: subscriber lagged, dropped {n} events");
                }
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fan_out_to_all_subscribers() {
        let bus = Bus::new(8);
        let mut a = bus.subscribe();
        let mut b = bus.subscribe();
        bus.publish(1);
        bus.publish(2);
        assert_eq!((a.recv().await, a.recv().await), (Some(1), Some(2)));
        assert_eq!(b.recv().await, Some(1));
    }

    #[tokio::test]
    async fn closes_when_publishers_drop() {
        let bus = Bus::<u8>::new(4);
        let mut s = bus.subscribe();
        drop(bus);
        assert_eq!(s.recv().await, None);
    }
}

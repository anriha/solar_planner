use kameo::{Actor, message::Message};
use tokio::sync::mpsc;

use crate::{
    directives::Directives,
    protocol::{ServerMsg, StatePayload},
    telemetry::Snapshot,
};

#[derive(Actor, Default)]
pub struct StateActor {
    payload: StatePayload,
    directives: Directives,
    subscribers: Vec<(u64, mpsc::Sender<ServerMsg>)>,
    next_id: u64,
}

pub enum Update {
    Good(Snapshot),
    Down,
    Directives(Directives),
}

pub struct Subscribe {
    pub tx: mpsc::Sender<ServerMsg>,
}

pub struct Unsubscribe(pub u64);

pub struct GetState;

impl StateActor {
    fn broadcast(&mut self) {
        let msg = ServerMsg::state(&self.payload, &self.directives);
        self.subscribers
            .retain(|(_, tx)| tx.try_send(msg.clone()).is_ok());
    }
}

impl Message<Update> for StateActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: Update,
        _ctx: &mut kameo::prelude::Context<Self, Self::Reply>,
    ) -> Self::Reply {
        match msg {
            Update::Good(snap) => {
                self.payload = StatePayload::from_snapshot(snap);
                self.broadcast();
            }
            Update::Down => {
                if self.payload.link_ok {
                    self.payload.link_ok = false;
                    self.broadcast();
                }
            }
            Update::Directives(d) => {
                if d != self.directives {
                    self.directives = d;
                    self.broadcast();
                }
            }
        }
    }
}

impl Message<Subscribe> for StateActor {
    type Reply = u64;

    async fn handle(
        &mut self,
        msg: Subscribe,
        _ctx: &mut kameo::prelude::Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let id = self.next_id;
        self.next_id += 1;
        self.subscribers.push((id, msg.tx));
        id
    }
}

impl Message<Unsubscribe> for StateActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: Unsubscribe,
        _ctx: &mut kameo::prelude::Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.subscribers.retain(|(id, _)| *id != msg.0);
    }
}

impl Message<GetState> for StateActor {
    type Reply = StatePayload;

    async fn handle(
        &mut self,
        _msg: GetState,
        _ctx: &mut kameo::prelude::Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.payload.clone()
    }
}

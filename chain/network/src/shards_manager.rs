use std::time::Instant;

use actix::Message;
use unc_primitives::{hash::CryptoHash, sharding::PartialEncodedChunk};

use crate::types::{
    PartialEncodedChunkForwardMsg, PartialEncodedChunkRequestMsg, PartialEncodedChunkResponseMsg,
};

#[derive(Message, Debug, strum::IntoStaticStr)]
#[rtype(result = "()")]
pub enum ShardsManagerRequestFromNetwork {
    ProcessPartialEncodedChunk(PartialEncodedChunk),
    ProcessPartialEncodedChunkForward(PartialEncodedChunkForwardMsg),
    ProcessPartialEncodedChunkResponse {
        partial_encoded_chunk_response: PartialEncodedChunkResponseMsg,
        received_time: Instant,
    },
    ProcessPartialEncodedChunkRequest {
        partial_encoded_chunk_request: PartialEncodedChunkRequestMsg,
        route_back: CryptoHash,
    },
}

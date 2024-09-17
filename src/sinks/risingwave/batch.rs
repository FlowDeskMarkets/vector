use crate::codecs::Encoder;
use crate::sinks::prelude::*;
use vector_lib::stream::batcher::limiter::ItemBatchSize;

#[derive(Default)]
pub(super) struct RisingWaveBatchSizer {
    pub(super) encoder: Encoder<()>,
}

impl RisingWaveBatchSizer {
    pub(super) fn estimated_size_of(&self, event: &Event) -> usize {
        match self.encoder.serializer() {
            vector_lib::codecs::encoding::Serializer::Json(_)
            | vector_lib::codecs::encoding::Serializer::NativeJson(_) => {
                event.estimated_json_encoded_size_of().get()
            }
            _ => event.size_of(),
        }
    }
}

impl ItemBatchSize<Event> for RisingWaveBatchSizer {
    fn size(&self, event: &Event) -> usize {
        self.estimated_size_of(event)
    }
}

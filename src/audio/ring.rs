// Type aliases for the SPSC ring buffer that carries interleaved f32 samples
// from the engine thread to the cpal callback. Producer lives on the engine,
// consumer is moved into the cpal output callback.

pub type SampleProducer = ringbuf::HeapProd<f32>;
pub type SampleConsumer = ringbuf::HeapCons<f32>;

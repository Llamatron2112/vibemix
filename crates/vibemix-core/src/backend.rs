pub type PortId = u32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Input,
    Output,
}

pub trait AudioBackend {
    type Error: std::error::Error + Send + Sync + 'static;

    fn name(&self) -> &'static str;
    fn sample_rate(&self) -> u32;
    fn buffer_size(&self) -> u32;
    fn register_audio_port(&mut self, name: &str, direction: Direction)
        -> Result<PortId, Self::Error>;
    fn connect_ports_by_name(&mut self, source: &str, destination: &str) -> Result<(), Self::Error>;
    fn activate(&mut self) -> Result<(), Self::Error>;
    fn deactivate(&mut self) -> Result<(), Self::Error>;
}

pub struct AudioBlock<'a> {
    pub nframes: usize,
    pub inputs: &'a [&'a [f32]],
    pub outputs: &'a mut [&'a mut [f32]],
}

pub trait ProcessHandler {
    fn process(&mut self, block: &mut AudioBlock<'_>);
}

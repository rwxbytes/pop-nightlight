use crate::backend::DisplayBackend;
use crate::color::RenderColor;
use anyhow::Result;

#[derive(Default)]
pub struct NoopBackend {
    pub last_applied: Option<RenderColor>,
    pub reset_count: usize,
}

impl DisplayBackend for NoopBackend {
    fn name(&self) -> &'static str {
        "noop"
    }

    fn apply(&mut self, color: RenderColor) -> Result<()> {
        self.last_applied = Some(color);
        Ok(())
    }

    fn reset(&mut self) -> Result<()> {
        self.last_applied = Some(RenderColor::NEUTRAL);
        self.reset_count += 1;
        Ok(())
    }
}

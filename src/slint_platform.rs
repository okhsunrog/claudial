extern crate alloc;

use alloc::rc::Rc;
use slint::platform::software_renderer::MinimalSoftwareWindow;

pub struct EspPlatform {
    window: Rc<MinimalSoftwareWindow>,
}

impl EspPlatform {
    pub fn new(window: Rc<MinimalSoftwareWindow>) -> Self {
        Self { window }
    }
}

impl slint::platform::Platform for EspPlatform {
    fn create_window_adapter(
        &self,
    ) -> Result<Rc<dyn slint::platform::WindowAdapter>, slint::PlatformError> {
        Ok(self.window.clone())
    }

    fn duration_since_start(&self) -> core::time::Duration {
        core::time::Duration::from_millis(
            esp_hal::time::Instant::now()
                .duration_since_epoch()
                .as_millis(),
        )
    }
}

pub struct WidgetFactory;

impl WidgetFactory {
    pub fn build(&self) -> u32 {
        WIDGET_LIMIT
    }
}

pub const WIDGET_LIMIT: u32 = 10;

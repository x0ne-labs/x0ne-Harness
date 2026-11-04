mod config;
use config::WidgetFactory;

fn main() {
    let f = WidgetFactory;
    println!("{}", f.build());
}

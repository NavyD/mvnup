pub mod site;
pub mod util;

pub static CRATE_NAME: &str = env!("CARGO_PKG_NAME");

#[cfg(test)]
pub mod tests {
    use log::LevelFilter;
    use std::sync::Once;

    static INIT: Once = Once::new();

    #[cfg(test)]
    #[ctor::ctor]
    fn init() {
        use crate::CRATE_NAME;

        INIT.call_once(|| {
            env_logger::builder()
                .is_test(true)
                .filter_level(LevelFilter::Info)
                .filter_module(CRATE_NAME, LevelFilter::Trace)
                .init();
        });
    }
}

#[gpui::test]
fn eviction_returns_cached_data_without_starting_a_load(cx: &gpui::TestAppContext) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static LOADS: AtomicUsize = AtomicUsize::new(0);
    struct Cached;
    impl gpui::Asset for Cached {
        type Source = u8;
        type Output = u8;
        fn load(
            source: u8,
            _: &mut gpui::App,
        ) -> impl std::future::Future<Output = u8> + Send + 'static {
            LOADS.fetch_add(1, Ordering::Relaxed);
            let value = source;
            async move {
                if value == 0 {
                    std::future::pending().await
                } else {
                    value
                }
            }
        }
    }
    let initial = LOADS.load(Ordering::Relaxed);
    cx.update(|cx| {
        assert_eq!(cx.take_asset::<Cached>(&1), None);
        assert_eq!(LOADS.load(Ordering::Relaxed), initial);
        assert_eq!(cx.fetch_asset::<Cached>(&1), None);
        assert_eq!(cx.fetch_asset::<Cached>(&0), None);
    });
    cx.run_until_parked();
    cx.update(|cx| {
        assert_eq!(cx.take_asset::<Cached>(&1), Some(1));
        assert_eq!(cx.take_asset::<Cached>(&0), None);
        assert!(!cx.has_asset::<Cached>(&0), "pending entry must be removed");
        assert!(!cx.has_asset::<Cached>(&1), "loaded entry must be removed");
        assert_eq!(cx.take_asset::<Cached>(&1), None);
        assert_eq!(LOADS.load(Ordering::Relaxed), initial + 2);
    });
}

use lean_indicators::window::RollingWindow;

#[test]
fn rolling_window_preserves_newest_order_and_capacity() {
    // Mirrors LEAN RollingWindowTests: index 0 is newest and pushing past
    // capacity evicts the oldest item.
    let mut window = RollingWindow::new(3);

    window.push(1);
    window.push(2);
    window.push(3);

    assert_eq!(window.len(), 3);
    assert!(window.is_full());
    assert_eq!(window.newest(), Some(&3));
    assert_eq!(window.oldest(), Some(&1));
    assert_eq!(window.get(0), Some(&3));
    assert_eq!(window.get(1), Some(&2));
    assert_eq!(window.get(2), Some(&1));

    window.push(4);

    assert_eq!(window.len(), 3);
    assert_eq!(window.iter().copied().collect::<Vec<_>>(), vec![4, 3, 2]);
    assert_eq!(window.get(3), None);
}

#[test]
fn rolling_window_clear_resets_count_without_changing_capacity() {
    let mut window = RollingWindow::new(2);
    window.push("old");
    window.push("new");

    window.clear();

    assert!(window.is_empty());
    assert!(!window.is_full());
    assert_eq!(window.capacity(), 2);
    assert_eq!(window.newest(), None);
    assert_eq!(window.oldest(), None);
}

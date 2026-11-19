use super::*;

#[test]
fn downscales_rgba_texture_to_maximum_dimension() {
    let pixels = (0..32).map(|value| value as u8).collect::<Vec<_>>();
    let (width, height, scaled) = downscale_rgba(4, 2, &pixels, 2).unwrap();

    assert_eq!((width, height), (2, 1));
    assert_eq!(scaled, [0, 1, 2, 3, 8, 9, 10, 11]);
}

#[test]
fn rejects_zero_sized_rgba_texture() {
    assert!(downscale_rgba(0, 2, &[], 1).is_err());
}

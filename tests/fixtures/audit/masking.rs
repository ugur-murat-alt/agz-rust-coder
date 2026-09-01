pub fn masking() {
    // None::<u8>.unwrap(); value.clone(); unsafe { }
    /* std::sync::Mutex; async { }; Arc<Mutex<u8>> */
    let _normal = "None::<u8>.unwrap(); value.clone(); unsafe { }";
    let _bytes = b"std::sync::Mutex; Arc<Mutex<u8>>";
    let _raw = r#"None::<u8>.unwrap(); unsafe { }"#;
    let _raw_bytes = br##"for i in 0..values.len() { }"##;
}

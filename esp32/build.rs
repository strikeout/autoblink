use std::io::Write;

fn main() {
    embuild::espidf::sysenv::output();

    // The firmware serves the page with gzip. The compressed page is about a third of the size.
    // Thus a page load needs fewer WiFi transmit buffers, and the image needs less flash.
    let page = std::fs::read("../webui/index.html").expect("webui/index.html is missing");
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("index.html.gz");
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
    gz.write_all(&page).unwrap();
    std::fs::write(out, gz.finish().unwrap()).unwrap();
    println!("cargo:rerun-if-changed=../webui/index.html");
}

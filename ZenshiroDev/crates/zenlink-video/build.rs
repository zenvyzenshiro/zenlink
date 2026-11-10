fn main() {
    #[cfg(target_os = "linux")]
    {
        let mut build = cc::Build::new();
        build.file("src/zlvideo.c");

        let mut cfg = pkg_config::Config::new();
        cfg.cargo_metadata(false); // DO NOT link directly to libavcodec! We use dlopen for multi-version FFmpeg support (FFmpeg 4, 5, 6, 7+)

        if let Ok(codec) = cfg.probe("libavcodec") {
            for inc in &codec.include_paths {
                build.include(inc);
            }
        }
        if let Ok(swscale) = cfg.probe("libswscale") {
            for inc in &swscale.include_paths {
                build.include(inc);
            }
        }
        if let Ok(avutil) = cfg.probe("libavutil") {
            for inc in &avutil.include_paths {
                build.include(inc);
            }
        }

        build.compile("zlvideo");
        println!("cargo:rustc-link-lib=dl");
        println!("cargo:rerun-if-changed=src/zlvideo.c");
    }
}

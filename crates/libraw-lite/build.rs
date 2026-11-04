const SOURCES: &[&str] = &[
    "LibRaw/src/decoders/canon_600.cpp",
    "LibRaw/src/decoders/crx.cpp",
    "LibRaw/src/decoders/decoders_dcraw.cpp",
    "LibRaw/src/decoders/decoders_libraw.cpp",
    "LibRaw/src/decoders/decoders_libraw_dcrdefs.cpp",
    "LibRaw/src/decoders/dng.cpp",
    "LibRaw/src/decoders/fp_dng.cpp",
    "LibRaw/src/decoders/fuji_compressed.cpp",
    "LibRaw/src/decoders/generic.cpp",
    "LibRaw/src/decoders/kodak_decoders.cpp",
    "LibRaw/src/decoders/load_mfbacks.cpp",
    "LibRaw/src/decoders/olympus14.cpp",
    "LibRaw/src/decoders/pana8.cpp",
    "LibRaw/src/decoders/smal.cpp",
    "LibRaw/src/decoders/sonycc.cpp",
    "LibRaw/src/decoders/unpack.cpp",
    "LibRaw/src/decoders/unpack_thumb.cpp",
    "LibRaw/src/decompressors/losslessjpeg.cpp",
    "LibRaw/src/demosaic/aahd_demosaic.cpp",
    "LibRaw/src/demosaic/ahd_demosaic.cpp",
    "LibRaw/src/demosaic/dcb_demosaic.cpp",
    "LibRaw/src/demosaic/dht_demosaic.cpp",
    "LibRaw/src/demosaic/misc_demosaic.cpp",
    "LibRaw/src/demosaic/xtrans_demosaic.cpp",
    "LibRaw/src/integration/dngsdk_glue.cpp",
    "LibRaw/src/integration/rawspeed_glue.cpp",
    "LibRaw/src/metadata/adobepano.cpp",
    "LibRaw/src/metadata/canon.cpp",
    "LibRaw/src/metadata/ciff.cpp",
    "LibRaw/src/metadata/cr3_parser.cpp",
    "LibRaw/src/metadata/epson.cpp",
    "LibRaw/src/metadata/exif_gps.cpp",
    "LibRaw/src/metadata/fuji.cpp",
    "LibRaw/src/metadata/hasselblad_model.cpp",
    "LibRaw/src/metadata/identify.cpp",
    "LibRaw/src/metadata/identify_tools.cpp",
    "LibRaw/src/metadata/kodak.cpp",
    "LibRaw/src/metadata/leica.cpp",
    "LibRaw/src/metadata/makernotes.cpp",
    "LibRaw/src/metadata/mediumformat.cpp",
    "LibRaw/src/metadata/minolta.cpp",
    "LibRaw/src/metadata/misc_parsers.cpp",
    "LibRaw/src/metadata/nikon.cpp",
    "LibRaw/src/metadata/normalize_model.cpp",
    "LibRaw/src/metadata/olympus.cpp",
    "LibRaw/src/metadata/p1.cpp",
    "LibRaw/src/metadata/pentax.cpp",
    "LibRaw/src/metadata/samsung.cpp",
    "LibRaw/src/metadata/sony.cpp",
    "LibRaw/src/metadata/tiff.cpp",
    "LibRaw/src/postprocessing/aspect_ratio.cpp",
    "LibRaw/src/postprocessing/dcraw_process.cpp",
    "LibRaw/src/postprocessing/mem_image.cpp",
    "LibRaw/src/postprocessing/postprocessing_aux.cpp",
    "LibRaw/src/postprocessing/postprocessing_utils.cpp",
    "LibRaw/src/postprocessing/postprocessing_utils_dcrdefs.cpp",
    "LibRaw/src/preprocessing/ext_preprocess.cpp",
    "LibRaw/src/preprocessing/raw2image.cpp",
    "LibRaw/src/preprocessing/subtract_black.cpp",
    "LibRaw/src/tables/cameralist.cpp",
    "LibRaw/src/tables/colorconst.cpp",
    "LibRaw/src/tables/colordata.cpp",
    "LibRaw/src/tables/wblists.cpp",
    "LibRaw/src/utils/curves.cpp",
    "LibRaw/src/utils/decoder_info.cpp",
    "LibRaw/src/utils/init_close_utils.cpp",
    "LibRaw/src/utils/open.cpp",
    "LibRaw/src/utils/phaseone_processing.cpp",
    "LibRaw/src/utils/read_utils.cpp",
    "LibRaw/src/utils/thumb_utils.cpp",
    "LibRaw/src/utils/utils_dcraw.cpp",
    "LibRaw/src/utils/utils_libraw.cpp",
    "LibRaw/src/write/apply_profile.cpp",
    "LibRaw/src/write/file_write.cpp",
    "LibRaw/src/write/tiff_writer.cpp",
    "LibRaw/src/x3f/x3f_parse_process.cpp",
    "LibRaw/src/x3f/x3f_utils_patched.cpp",
    "LibRaw/src/libraw_c_api.cpp",
    "LibRaw/src/libraw_datastream.cpp",
];

fn main() {
    let mut b = cc::Build::new();
    let msvc = b.get_compiler().is_like_msvc();
    b.cpp(true).include("LibRaw").file("shim.cpp");
    for f in SOURCES {
        b.file(f);
    }
    b.warnings(false).extra_warnings(false);
    b.define("LIBRAW_NODLL", None);
    b.define("_USE_MATH_DEFINES", None);
    if msvc {
        b.define("_CRT_SECURE_NO_WARNINGS", None);
        b.flag("/EHsc");
    } else {
        b.flag_if_supported("-std=c++14");
        b.flag_if_supported("-Wno-format-truncation");
        b.flag_if_supported("-Wno-unused-result");
        b.flag_if_supported("-Wno-format-overflow");
        b.flag_if_supported("-pthread");
    }
    b.compile("raw");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo:rustc-link-lib=ws2_32");
    }
    println!("cargo:rerun-if-changed=LibRaw");
    println!("cargo:rerun-if-changed=shim.cpp");
    println!("cargo:rerun-if-changed=build.rs");
}

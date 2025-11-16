use std::{
    io::{BufReader, BufWriter, Cursor},
    path::PathBuf,
};

use byteorder::LittleEndian;
use clap::Parser;
use color_eyre::{
    Result,
    eyre::{Context, eyre},
};
use tracing::Level;
use tracing_subscriber::fmt;
use unrealin::{
    ExportedData,
    de::{self, LinearFileDecoder},
};

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// Where to extract files to. By default this will be the basename of the input file.
    /// For example, `common.lin` will extract to `common/`
    #[arg(short, long)]
    output: Option<PathBuf>,

    /// File to extract
    common_lin: PathBuf,

    map_lin: PathBuf,
}
fn main() -> Result<()> {
    let mut args = Args::parse();

    let subscriber = fmt().with_max_level(Level::TRACE).finish();
    tracing::subscriber::set_global_default(subscriber).expect("setting default subscriber failed");

    let mut common_file = std::fs::File::open(&args.common_lin)
        .wrap_err_with(|| format!("failed to open {:?}", &args.common_lin))?;
    let mut common_mmap = unsafe { memmap2::Mmap::map(&common_file)? };
    let mut raw_common_file = &common_mmap[..];

    let mut map_file = std::fs::File::open(&args.map_lin)
        .wrap_err_with(|| format!("failed to open {:?}", &args.map_lin))?;
    let mut map_mmap = unsafe { memmap2::Mmap::map(&map_file)? };
    let mut raw_map_file = &map_mmap[..];

    let mut output_dir = if let Some(output_dir) = args.output.take() {
        output_dir
    } else {
        let Some(parent) = args.common_lin.parent() else {
            return Err(eyre!("Input path {:?} has no parent", args.common_lin));
        };

        let Some(stem) = args.common_lin.file_stem() else {
            return Err(eyre!("Input path {:?} has no file stem", args.common_lin));
        };

        parent.join(stem)
    };

    std::fs::create_dir_all(&output_dir)
        .wrap_err_with(|| format!("failed to create output dir {:?}", &output_dir))?;

    let output_path = output_dir.join("complete.bin");
    let mut out_file = BufWriter::new(
        std::fs::File::create(&output_path)
            .wrap_err_with(|| format!("failed to create output file {output_path:?}"))?,
    );

    let common_lin_data = if args
        .common_lin
        .extension()
        .as_ref()
        .map(|ext| ext.to_str().unwrap() == "lin")
        .unwrap_or_default()
    {
        unrealin::de::decompress_linear_file::<LittleEndian, _>(&mut raw_common_file)?
    } else {
        raw_common_file.to_vec()
    };

    let map_lin_data = if args
        .common_lin
        .extension()
        .as_ref()
        .map(|ext| ext.to_str().unwrap() == "lin")
        .unwrap_or_default()
    {
        unrealin::de::decompress_linear_file::<LittleEndian, _>(&mut raw_map_file)?
    } else {
        raw_common_file.to_vec()
    };

    std::io::copy(&mut common_lin_data.as_slice(), &mut out_file)
        .wrap_err_with(|| format!("failed to copy data to output file {output_path:?}"))?;

    let reader = BufReader::new(
        std::fs::File::open("/var/tmp/reads.json").expect("failed to open reads file"),
    );

    let mut metadata: ExportedData = serde_json::from_reader(reader).expect("failed to parse read");
    metadata.file_ptr_order.reverse();
    metadata
        .file_reads
        .iter_mut()
        .for_each(|(_k, v)| v.reverse());

    let mut lin_decoder = LinearFileDecoder::<LittleEndian, _>::new_checked(
        vec![Cursor::new(common_lin_data), Cursor::new(map_lin_data)],
        metadata,
    );
    lin_decoder
        .decode_linear_file()
        .expect("failed to decode lienar file");

    // for (i, package) in linear_file.packages_mut().iter_mut().enumerate() {
    //     let out_path = output_dir.join(format!("{i}.bin"));
    //     println!("Rewriting {:?}", out_path);
    //     let mut writer = std::fs::File::create(&out_path)?;
    //     unrealin::ser::serialize_unreal_package(writer, package)
    //         .expect("failed to serialize package");

    //     let reader = std::fs::read(&out_path).unwrap();
    //     let mut input = reader.as_ref();
    //     le_u32::<_, ContextError>(&mut input);
    //     let res = de::read_package(&mut input).unwrap();
    //     for (i, export) in res.exports.iter().enumerate() {
    //         if export.object_name < 0 || export.object_name as usize >= res.names.len() {
    //             println!("Prev: {:#X?}", res.exports[i - 1]);
    //             panic!("Bad export: {i} {:#X?}", export);
    //         }
    //     }
    // }

    Ok(())
}

use std::path::PathBuf;

use clap::Parser;
use color_eyre::{
    Result,
    eyre::{Context, eyre},
};

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// Where to extract files to. By default this will be the basename of the input file.
    /// For example, `common.lin` will extract to `common/`
    #[arg(short, long)]
    output: Option<PathBuf>,

    /// File to extract
    input: PathBuf,
}
fn main() -> Result<()> {
    let mut args = Args::parse();

    let mut input_file = std::fs::File::open(&args.input)
        .wrap_err_with(|| format!("failed to open {:?}", &args.input))?;

    let mut mmap = unsafe { memmap2::Mmap::map(&input_file)? };

    let mut raw_linear_file = &mmap[..];

    let mut output_dir = if let Some(output_dir) = args.output.take() {
        output_dir
    } else {
        let Some(parent) = args.input.parent() else {
            return Err(eyre!("Input path {:?} has no parent", args.input));
        };

        let Some(stem) = args.input.file_stem() else {
            return Err(eyre!("Input path {:?} has no file stem", args.input));
        };

        parent.join(stem)
    };

    std::fs::create_dir_all(&output_dir)
        .wrap_err_with(|| format!("failed to create output dir {:?}", &output_dir))?;

    let output_path = output_dir.join("complete.bin");
    let mut out_file = std::fs::File::create(&output_path)
        .wrap_err_with(|| format!("failed to create output file {output_path:?}"))?;

    let out_data = if args
        .input
        .extension()
        .as_ref()
        .map(|ext| ext.to_str().unwrap() == "lin")
        .unwrap_or_default()
    {
        unrealin::de::decompress_linear_file(raw_linear_file)
    } else {
        raw_linear_file.to_vec()
    };

    std::io::copy(&mut out_data.as_slice(), &mut out_file)
        .wrap_err_with(|| format!("failed to copy data to output file {output_path:?}"))?;

    let linear_file = unrealin::de::decode_linear_file(out_data.as_slice());

    for (i, package) in linear_file.packages().iter().enumerate() {
        let mut writer = std::fs::File::create(output_dir.join(format!("{i}.bin")))?;
        unrealin::ser::serialize_unreal_package(writer, package);
    }

    Ok(())
}

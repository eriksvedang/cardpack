mod images;
mod pack;

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Parser, ValueEnum};
use lopdf::{Dictionary, Document, Object, Stream, dictionary};

use images::ImageInfo;

const PT_PER_MM: f64 = 72.0 / 25.4;

#[derive(Clone, Copy, Debug, ValueEnum)]
enum PageSize {
    A2,
    A3,
    A4,
}

impl PageSize {
    /// Portrait size in millimetres.
    fn mm(self) -> (f64, f64) {
        match self {
            PageSize::A2 => (420.0, 594.0),
            PageSize::A3 => (297.0, 420.0),
            PageSize::A4 => (210.0, 297.0),
        }
    }
}

/// Pack a directory of images onto as few A2/A3/A4 PDF pages as possible.
#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
    /// Directory containing the images
    input: PathBuf,

    /// Output PDF file
    #[arg(short, long, default_value = "cardpack.pdf")]
    output: PathBuf,

    /// Page size
    #[arg(short, long, value_enum, default_value_t = PageSize::A4)]
    page: PageSize,

    /// Use landscape pages
    #[arg(short, long)]
    landscape: bool,

    /// Page margin in mm
    #[arg(short, long, default_value_t = 10.0)]
    margin: f64,

    /// Space between images in mm. Negative values make neighbouring images
    /// overlap by that much
    #[arg(short, long, default_value_t = 2.0, allow_negative_numbers = true)]
    gap: f64,

    /// Resolution used to turn pixels into physical size (ignored if --width/--height is set)
    #[arg(long, default_value_t = 300.0)]
    dpi: f64,

    /// Print every image at this width in mm. With --height too, images are
    /// scaled to fit inside that box (aspect ratio is always kept)
    #[arg(long)]
    width: Option<f64>,

    /// Print every image at this height in mm
    #[arg(long)]
    height: Option<f64>,

    /// Never rotate images to make them fit better
    #[arg(long)]
    no_rotate: bool,

    /// Draw a thin outline around each image (handy as a cutting guide)
    #[arg(long)]
    outline: bool,

    /// Also pick up images in subdirectories
    #[arg(short, long)]
    recursive: bool,

    /// File listing which images to include and how many copies of each, one
    /// per line as `<count> <name>` (e.g. `4 FiveOfClubs`). The name is the
    /// file name, with or without its extension; a missing count means 1.
    /// Images not listed are left out
    #[arg(short, long)]
    counts: Option<PathBuf>,
}

fn main() -> Result<()> {
    let args = Args::parse();
    if !(args.margin >= 0.0) {
        bail!("--margin must be zero or positive");
    }
    if !args.gap.is_finite() {
        bail!("--gap must be a number");
    }
    for (name, v) in [("dpi", Some(args.dpi)), ("width", args.width), ("height", args.height)] {
        if v.is_some_and(|v| !(v > 0.0)) {
            bail!("--{name} must be positive");
        }
    }

    let (mut page_w, mut page_h) = args.page.mm();
    if args.landscape {
        (page_w, page_h) = (page_h, page_w);
    }
    let (page_w, page_h) = (page_w * PT_PER_MM, page_h * PT_PER_MM);
    let margin = args.margin * PT_PER_MM;
    let gap = args.gap * PT_PER_MM;
    let (area_w, area_h) = (page_w - 2.0 * margin, page_h - 2.0 * margin);
    if area_w <= 0.0 || area_h <= 0.0 {
        bail!("margin of {} mm leaves no room on the page", args.margin);
    }

    let mut paths = Vec::new();
    collect_images(&args.input, args.recursive, &mut paths)
        .with_context(|| format!("reading {}", args.input.display()))?;
    paths.sort();

    // Pair each image with the number of copies wanted.
    let paths: Vec<(PathBuf, usize)> = match &args.counts {
        Some(file) => {
            let counts = read_counts(file).with_context(|| format!("reading {}", file.display()))?;
            let mut used = vec![false; counts.len()];
            let selected = paths
                .into_iter()
                .filter_map(|path| {
                    let i = counts.iter().position(|(name, _)| matches_name(&path, name))?;
                    used[i] = true;
                    Some((path, counts[i].1))
                })
                .collect();
            for ((name, _), used) in counts.iter().zip(used) {
                if !used {
                    eprintln!("warning: no image named {name} in {}", args.input.display());
                }
            }
            selected
        }
        None => paths
            .into_iter()
            .map(|path| {
                let n = copies(&path);
                (path, n)
            })
            .collect(),
    };

    let mut infos = Vec::new();
    let mut counts = Vec::new();
    for (path, count) in paths {
        if count == 0 {
            continue;
        }
        match images::probe(&path) {
            Ok(info) => {
                infos.push(info);
                counts.push(count);
            }
            Err(e) => eprintln!("skipping {}: {e}", path.display()),
        }
    }
    if infos.is_empty() {
        bail!("no images found in {}", args.input.display());
    }

    let info_sizes: Vec<(f64, f64)> = infos
        .iter()
        .map(|info| {
            let (w, h) = print_size(info, &args);
            fit(info, w, h, area_w, area_h, !args.no_rotate)
        })
        .collect();
    if let Some((info, _)) = infos.iter().zip(&info_sizes).find(|(_, (w, h))| w.min(*h) + gap <= 0.0) {
        bail!(
            "--gap of {} mm would make {} overlap itself; it must be larger than minus the image's shortest side",
            args.gap,
            info.path.display()
        );
    }

    // One item per copy; `items[i]` is the index into `infos` of item `i`.
    let items: Vec<usize> = counts
        .iter()
        .enumerate()
        .flat_map(|(i, &n)| std::iter::repeat(i).take(n))
        .collect();
    let sizes: Vec<(f64, f64)> = items.iter().map(|&i| info_sizes[i]).collect();

    let pages = pack::pack(&sizes, area_w, area_h, gap, !args.no_rotate);

    let mut doc = Document::with_version("1.5");
    let pages_id = doc.new_object_id();
    let mut kids = Vec::new();
    // Copies of an image share a single embedded XObject.
    let mut image_ids = vec![None; infos.len()];

    for (n, page) in pages.iter().enumerate() {
        // Centre the packed block within the printable area.
        let (used_w, used_h) = page.iter().fold((0.0f64, 0.0f64), |(mw, mh), p| {
            let (w, h) = placed_size(sizes[p.index], p.rotated);
            (mw.max(p.x + w), mh.max(p.y + h))
        });
        let off_x = margin + (area_w - used_w) / 2.0;
        let off_y = margin + (area_h - used_h) / 2.0;

        let mut content = String::new();
        let mut xobjects = Dictionary::new();
        for (k, p) in page.iter().enumerate() {
            let info_index = items[p.index];
            let image_id = match image_ids[info_index] {
                Some(id) => id,
                None => {
                    let info = &infos[info_index];
                    let id = images::embed(&mut doc, info)
                        .with_context(|| format!("embedding {}", info.path.display()))?;
                    image_ids[info_index] = Some(id);
                    id
                }
            };
            let name = format!("Im{k}");
            xobjects.set(name.as_bytes(), Object::Reference(image_id));

            let (w, h) = sizes[p.index];
            let (pw, ph) = placed_size((w, h), p.rotated);
            // Packer coordinates run top-down; PDF runs bottom-up.
            let x = off_x + p.x;
            let y = page_h - off_y - p.y - ph;
            let matrix = if p.rotated {
                // Turned 90° counter-clockwise into a pw × ph slot.
                [0.0, ph, -pw, 0.0, x + pw, y]
            } else {
                [w, 0.0, 0.0, h, x, y]
            };
            let [a, b, c, d, e, f] = matrix;
            writeln!(content, "q {a:.4} {b:.4} {c:.4} {d:.4} {e:.4} {f:.4} cm /{name} Do Q")?;
            if args.outline {
                writeln!(content, "q 0.25 w 0.6 G {x:.4} {y:.4} {pw:.4} {ph:.4} re S Q")?;
            }
        }

        let mut content = Stream::new(Dictionary::new(), content.into_bytes());
        content.compress()?;
        let content_id = doc.add_object(content);
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "MediaBox" => vec![0.into(), 0.into(), page_w.into(), page_h.into()],
            "Contents" => content_id,
            "Resources" => dictionary! { "XObject" => xobjects },
        });
        kids.push(page_id.into());
        eprintln!("page {}: {} image(s)", n + 1, page.len());
    }

    doc.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages",
            "Count" => kids.len() as i64,
            "Kids" => kids,
        }),
    );
    let catalog_id = doc.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => pages_id,
    });
    doc.trailer.set("Root", catalog_id);
    doc.save(&args.output)
        .with_context(|| format!("writing {}", args.output.display()))?;

    eprintln!(
        "wrote {} image(s) on {} page(s) to {}",
        items.len(),
        pages.len(),
        args.output.display()
    );
    Ok(())
}

fn collect_images(dir: &Path, recursive: bool, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            if recursive {
                collect_images(&path, recursive, out)?;
            }
        } else if path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| images::EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
        {
            out.push(path);
        }
    }
    Ok(())
}

/// Number of copies asked for by a `_x<N>` suffix on the file name, e.g. `card_x3.png`.
fn copies(path: &Path) -> usize {
    path.file_stem()
        .and_then(|s| s.to_str())
        .and_then(|s| s.rsplit_once("_x"))
        .map(|(_, n)| n)
        .filter(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|n| n.parse().ok())
        .filter(|&n| n >= 1)
        .unwrap_or(1)
}

/// Parses a counts file: one `<count> <name>` or `<name>` per line. Blank
/// lines and lines starting with `#` are ignored.
fn read_counts(file: &Path) -> Result<Vec<(String, usize)>> {
    let text = std::fs::read_to_string(file)?;
    let mut counts: Vec<(String, usize)> = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (count, name) = match line.split_once(char::is_whitespace) {
            Some((count, name)) if count.bytes().all(|b| b.is_ascii_digit()) => (
                count.parse().with_context(|| format!("line {}: bad count", n + 1))?,
                name.trim(),
            ),
            _ => (1, line),
        };
        if counts.iter().any(|(other, _)| other == name) {
            bail!("line {}: {name} is listed more than once", n + 1);
        }
        counts.push((name.to_owned(), count));
    }
    Ok(counts)
}

/// Whether `name` from a counts file refers to `path`, by file name with or without extension.
fn matches_name(path: &Path, name: &str) -> bool {
    path.file_stem().is_some_and(|s| s == name) || path.file_name().is_some_and(|s| s == name)
}

/// Physical size of an image in points, from --width/--height or --dpi.
fn print_size(info: &ImageInfo, args: &Args) -> (f64, f64) {
    let aspect = info.height as f64 / info.width as f64;
    let mm = match (args.width, args.height) {
        (Some(w), Some(h)) => {
            // Fit inside a w × h box; turn the box to match the image if rotation is allowed.
            let (bw, bh) = if !args.no_rotate && (aspect > 1.0) != (h / w > 1.0) {
                (h, w)
            } else {
                (w, h)
            };
            let s = (bw / info.width as f64).min(bh / info.height as f64);
            (info.width as f64 * s, info.height as f64 * s)
        }
        (Some(w), None) => (w, w * aspect),
        (None, Some(h)) => (h / aspect, h),
        (None, None) => {
            let mm_per_px = 25.4 / args.dpi;
            (info.width as f64 * mm_per_px, info.height as f64 * mm_per_px)
        }
    };
    (mm.0 * PT_PER_MM, mm.1 * PT_PER_MM)
}

/// Shrinks an image that is too big for the printable area, keeping its aspect ratio.
fn fit(info: &ImageInfo, w: f64, h: f64, area_w: f64, area_h: f64, rotate: bool) -> (f64, f64) {
    let scale_for = |w: f64, h: f64| (area_w / w).min(area_h / h);
    let mut scale = scale_for(w, h);
    if rotate {
        scale = scale.max(scale_for(h, w));
    }
    if scale >= 1.0 {
        return (w, h);
    }
    eprintln!(
        "warning: {} is larger than the page; scaling to {:.0}%",
        info.path.display(),
        scale * 100.0
    );
    // A hair smaller so floating-point error can't make it not fit.
    let scale = scale * (1.0 - 1e-9);
    (w * scale, h * scale)
}

fn placed_size((w, h): (f64, f64), rotated: bool) -> (f64, f64) {
    if rotated { (h, w) } else { (w, h) }
}

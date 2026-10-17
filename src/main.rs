//! conv: convert any file into any relevant format by chaining the converters already installed.
//!
//! Each `Edge` states "tool T turns formats A into formats B". A breadth-first search over
//! formats finds the shortest chain whose tools exist, e.g. tex -latexmk-> pdf -pdftoppm-> png.
//! Ties go to the edge listed first, so the table below is also the quality/speed preference
//! order. Intermediates live in a per-job temp dir; jobs (input x target) run in parallel.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{exit, Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;
use std::time::Instant;
use std::{env, fs, thread};

const USAGE: &str = "\
conv: convert files by chaining the converters you already have

usage: conv [-f] [-n] [-o DIR] INPUT... FORMAT...
       conv [-f] INPUT... -j OUT.pdf   join inputs (any convertible type) in order
       conv INPUT...                   list what each INPUT can become

  conv notes.pdf md             -> notes.md
  conv notes.tex pdf md html    -> three outputs, built in parallel
  conv *.heic jpg               -> batch
  conv talk.mov mp4 gif mp3     -> remux/encode, gif, audio
  conv photos zip               -> folders <-> archives (zip tar tgz txz 7z)
  conv a.pdf b.docx c.heic -j all.pdf

  -o DIR   write outputs to DIR (default: next to each input)
  -f       overwrite existing outputs
  -n       print the conversion route without running it
  -j OUT   join all inputs into one OUT (pdf)
";

const SHEETS_PY: &str = include_str!("sheets.py");

// ---------- formats ----------

const IMG: &[&str] = &[
    "jpg", "png", "heic", "avif", "tiff", "gif", "bmp", "webp", "psd", "ico", "icns", "jp2", "tga",
    "exr", "jxl",
];
const RAW: &[&str] = &["dng", "cr2", "cr3", "nef", "arw", "raf", "orf", "rw2"];
const SIPS_OUT: &[&str] = &[
    "jpg", "png", "heic", "avif", "tiff", "gif", "bmp", "ico", "icns", "jp2", "tga", "psd", "exr",
    "pdf",
];
const VIDEO: &[&str] = &["mp4", "mov", "mkv", "webm", "avi", "m4v", "wmv", "flv", "mpg", "ts", "3gp"];
const AUDIO: &[&str] = &["mp3", "m4a", "aac", "wav", "flac", "ogg", "opus", "aiff", "wma", "caf"];
const AUDIO_OUT: &[&str] = &["mp3", "m4a", "aac", "wav", "flac", "ogg", "opus", "aiff"];
const SUBS: &[&str] = &["srt", "vtt", "ass"];
const ARCH: &[&str] = &["zip", "tar", "tgz", "txz", "tbz2", "7z"];
const PANDOC_IN: &[&str] = &[
    "md", "html", "tex", "docx", "odt", "rtf", "epub", "rst", "org", "ipynb", "typ", "pptx", "xlsx",
    "csv", "tsv", "txt", "fb2", "textile",
];
const PANDOC_OUT: &[&str] = &[
    "md", "html", "tex", "docx", "odt", "rtf", "epub", "rst", "org", "ipynb", "typ", "txt", "pptx",
    "adoc", "fb2",
];
const TEXTUTIL: &[&str] = &["txt", "html", "rtf", "rtfd", "doc", "docx", "odt", "wordml", "webarchive"];
const WRITER: &[&str] = &["doc", "docx", "odt", "rtf", "pages", "wpd"];
const CALC: &[&str] = &["xls", "xlsx", "ods", "csv", "numbers"];
const IMPRESS: &[&str] = &["ppt", "pptx", "odp", "key"];

/// Map spelling variants onto one routing name; the user's spelling is kept for the file name.
fn canon(ext: &str) -> String {
    let e = ext.trim_start_matches('.').to_lowercase();
    match e.as_str() {
        "jpeg" | "jpe" => "jpg",
        "tif" => "tiff",
        "heif" | "heics" => "heic",
        "htm" | "xhtml" => "html",
        "markdown" => "md",
        "latex" => "tex",
        "aif" => "aiff",
        "mpeg" => "mpg",
        "text" => "txt",
        "tar.gz" => "tgz",
        "tar.xz" => "txz",
        "tar.bz2" | "tbz" => "tbz2",
        "folder" => "dir",
        _ => return e,
    }
    .into()
}

/// (format, stem) of a path. Directories are "dir" unless they are document bundles (.rtfd).
fn detect(p: &Path) -> (String, String) {
    let name = p.file_name().map_or(String::new(), |n| n.to_string_lossy().into_owned());
    let lower = name.to_lowercase();
    for dbl in ["tar.gz", "tar.xz", "tar.bz2"] {
        if lower.ends_with(&format!(".{dbl}")) {
            return (canon(dbl), name[..name.len() - dbl.len() - 1].into());
        }
    }
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && !(p.is_dir() && canon(ext) != "rtfd") => {
            (canon(ext), stem.into())
        }
        _ => ((if p.is_dir() { "dir" } else { "" }).into(), name),
    }
}

/// Coarse kinds, used to keep `conv FILE` listings to sensible targets: chaining makes
/// e.g. svg -> png -> gif -> mp4 reachable, but nobody wants a logo as a video.
fn kinds(f: &str) -> Vec<&'static str> {
    let mut k = vec![];
    let is = |g: &[&str]| g.contains(&f);
    if is(IMG) || is(RAW) || is(&["svg", "eps", "ps"]) { k.push("image") }
    if is(VIDEO) || f == "gif" { k.push("video") }
    if is(AUDIO) { k.push("audio") }
    if is(SUBS) { k.push("subs") }
    if is(ARCH) || is(&["rar", "iso", "dir"]) { k.push("archive") }
    if is(CALC) || is(&["tsv", "json"]) { k.push("sheet") }
    if is(IMPRESS) { k.push("slides") }
    if f == "pdf" { k.push("pdf") }
    if k.is_empty() { k.push("doc") }
    k
}

fn relevant(from: &str, to: &str) -> bool {
    let to = kinds(to);
    kinds(from).into_iter().any(|k| {
        let ok: &[&str] = match k {
            "image" => &["image", "pdf"],
            "video" => &["video", "audio", "image", "subs"],
            "audio" => &["audio"],
            "subs" => &["subs"],
            "archive" => &["archive"],
            "sheet" => &["sheet", "doc", "pdf"],
            "slides" => &["slides", "pdf", "image", "doc"],
            "pdf" => &["doc", "image"],
            _ => &["doc", "pdf", "image", "slides"],
        };
        to.iter().any(|t| ok.contains(t))
    })
}

// ---------- tools ----------

const TOOLS: &[&str] = &[
    "sips", "magick", "ffmpeg", "cwebp", "dwebp", "rsvg-convert", "pandoc", "textutil", "soffice",
    "pdftotext", "pdftoppm", "pdftocairo", "pdftohtml", "latexmk", "tectonic", "pdflatex",
    "xelatex", "lualatex", "typst", "python3", "markitdown", "ebook-convert", "tar", "sh",
    "pdfunite", "qpdf", "gs",
];

fn which(tool: &str) -> Option<&'static PathBuf> {
    static FOUND: OnceLock<HashMap<&str, PathBuf>> = OnceLock::new();
    FOUND
        .get_or_init(|| {
            let dirs: Vec<PathBuf> = env::var_os("PATH").map_or(vec![], |p| env::split_paths(&p).collect());
            let mut m = HashMap::new();
            for &t in TOOLS {
                if let Some(p) = dirs.iter().map(|d| d.join(t)).find(|p| p.is_file()) {
                    m.insert(t, p);
                }
            }
            let lo = PathBuf::from("/Applications/LibreOffice.app/Contents/MacOS/soffice");
            if !m.contains_key("soffice") && lo.is_file() {
                m.insert("soffice", lo);
            }
            m
        })
        .get(tool)
}

fn has(tool: &str) -> bool {
    which(tool).is_some()
}

fn install_hint(tool: &str) -> &'static str {
    match tool {
        "soffice" => "brew install --cask libreoffice",
        "magick" => "brew install imagemagick",
        "pdftotext" | "pdftoppm" | "pdftocairo" | "pdftohtml" | "pdfunite" => "brew install poppler",
        "rsvg-convert" => "brew install librsvg",
        "cwebp" | "dwebp" => "brew install webp",
        "latexmk" | "tectonic" | "xelatex" => "brew install tectonic",
        "ebook-convert" => "brew install --cask calibre",
        "markitdown" => "pipx install markitdown",
        "sips" | "textutil" => "macOS only",
        "ffmpeg" => "brew install ffmpeg",
        "pandoc" => "brew install pandoc",
        "typst" => "brew install typst",
        _ => "",
    }
}

// ---------- commands ----------

struct Cmd {
    tool: &'static str,
    args: Vec<OsString>,
    cwd: Option<PathBuf>,
    /// Where the tool actually writes, when it picks the name itself; moved to `Step::dst`.
    out: Option<PathBuf>,
}

fn cmd(tool: &'static str) -> Cmd {
    Cmd { tool, args: vec![], cwd: None, out: None }
}

impl Cmd {
    fn a(mut self, x: impl AsRef<OsStr>) -> Self {
        self.args.push(x.as_ref().into());
        self
    }
    fn args<S: AsRef<OsStr>>(self, xs: impl IntoIterator<Item = S>) -> Self {
        xs.into_iter().fold(self, Cmd::a)
    }
    fn cwd(mut self, p: &Path) -> Self {
        self.cwd = Some(p.into());
        self
    }
    fn out(mut self, p: PathBuf) -> Self {
        self.out = Some(p);
        self
    }
}

fn cat(a: &str, b: &Path) -> OsString {
    let mut s = OsString::from(a);
    s.push(b);
    s
}

/// One hop of a route. `tmp` is a scratch dir private to this hop.
struct Step<'a> {
    src: &'a Path,
    dst: &'a Path,
    from: &'a str,
    to: &'a str,
    tmp: &'a Path,
}

impl Step<'_> {
    fn dir(&self) -> &Path {
        self.src.parent().unwrap_or(Path::new("."))
    }
    fn stem(&self) -> String {
        self.src.file_stem().map_or("out".into(), |s| s.to_string_lossy().into_owned())
    }
}

/// `needs` entries are "a|b": at least one of a, b must be installed.
/// `run` returns alternatives, tried in order until one succeeds.
/// `multi` edges may write several files (one per page) and therefore end a route.
struct Edge {
    needs: &'static [&'static str],
    from: Vec<&'static str>,
    to: Vec<&'static str>,
    multi: bool,
    run: fn(&Step) -> Vec<Cmd>,
}

impl Edge {
    fn usable(&self) -> bool {
        self.needs.iter().all(|n| n.split('|').any(has))
    }
    fn name(&self) -> &'static str {
        let alts = self.needs[0];
        alts.split('|').find(|t| has(t)).unwrap_or(alts.split('|').next().unwrap())
    }
}

fn edge(needs: &'static [&'static str], from: &[&[&'static str]], to: &[&[&'static str]], run: fn(&Step) -> Vec<Cmd>) -> Edge {
    Edge { needs, from: from.concat(), to: to.concat(), multi: false, run }
}

fn ff(s: &Step) -> Cmd {
    cmd("ffmpeg").args(["-hide_banner", "-loglevel", "error", "-y", "-i"]).a(s.src)
}

fn ff_video(s: &Step) -> Vec<Cmd> {
    let mut alts = vec![];
    let faststart: &[&str] = if s.to == "mkv" { &[] } else { &["-movflags", "+faststart"] };
    // Changing only the container is instant; if the codecs don't fit it, re-encode below.
    if VIDEO.contains(&s.from) && ["mp4", "mov", "m4v", "mkv"].contains(&s.to) {
        let copy: &[&str] = if s.to == "mkv" { &["-map", "0", "-c", "copy"] } else { &["-sn", "-c", "copy"] };
        alts.push(ff(s).args(copy).args(faststart).a(s.dst));
    }
    let enc: &[&str] = match s.to {
        "gif" => &["-vf", "fps=12,scale='min(720,iw)':-2:flags=lanczos,split[a][b];[a]palettegen[p];[b][p]paletteuse", "-loop", "0"],
        "webm" => &["-c:v", "libvpx-vp9", "-crf", "33", "-b:v", "0", "-row-mt", "1", "-cpu-used", "4", "-c:a", "libopus"],
        "avi" => &[],
        // h264 needs even dimensions (gif input often isn't) and yuv420p for broad playback.
        _ => &["-c:v", "libx264", "-preset", "veryfast", "-crf", "23", "-pix_fmt", "yuv420p",
               "-vf", "scale=trunc(iw/2)*2:trunc(ih/2)*2", "-c:a", "aac"],
    };
    alts.push(ff(s).args(enc).args(if s.to == "gif" { &[] } else { faststart }).a(s.dst));
    alts
}

fn ff_audio(s: &Step) -> Vec<Cmd> {
    let mut alts = vec![];
    if matches!(s.to, "m4a" | "aac") && matches!(s.from, "mp4" | "mov" | "m4v") {
        alts.push(ff(s).args(["-vn", "-c:a", "copy"]).a(s.dst)); // these usually already carry aac
    }
    let codec: &[&str] = match s.to {
        "mp3" => &["-c:a", "libmp3lame", "-q:a", "2"],
        "m4a" | "aac" => &["-c:a", "aac", "-b:a", "192k"],
        "opus" | "ogg" => &["-c:a", "libopus", "-b:a", "128k"],
        _ => &[],
    };
    alts.push(ff(s).a("-vn").args(codec).a(s.dst));
    alts
}

fn latex(s: &Step) -> Vec<Cmd> {
    let text = fs::read_to_string(s.src).unwrap_or_default();
    let xe = text.contains("fontspec") || text.contains("unicode-math");
    let flags = ["-interaction=nonstopmode", "-halt-on-error", "-file-line-error"];
    let out = s.tmp.join(format!("{}.pdf", s.stem()));
    [
        cmd("latexmk").a(if xe { "-xelatex" } else { "-pdf" }).args(flags).a(cat("-outdir=", s.tmp)).a(s.src),
        cmd("tectonic").a("-o").a(s.tmp).a(s.src),
        cmd(if xe { "xelatex" } else { "pdflatex" }).args(flags).a(cat("-output-directory=", s.tmp)).a(s.src),
    ]
    .into_iter()
    .map(|c| c.cwd(s.dir()).out(out.clone()))
    .collect()
}

fn pandoc_fmt(f: &str, out: bool) -> &str {
    match f {
        "md" if out => "commonmark_x", // gfm-like, but math stays $...$
        "txt" if out => "plain",
        "md" | "txt" => "markdown",
        "tex" => "latex",
        "typ" => "typst",
        "adoc" => "asciidoc",
        f => f,
    }
}

fn pandoc(s: &Step) -> Vec<Cmd> {
    let mut c = cmd("pandoc").a(s.src).args(["-f", pandoc_fmt(s.from, false), "-o"]).a(s.dst);
    c = c.a("--resource-path").a(s.dir()).cwd(s.dir());
    if s.to == "pdf" {
        let engine = ["xelatex", "lualatex", "tectonic", "pdflatex", "typst"].into_iter().find(|t| has(t)).unwrap_or("xelatex");
        c = c.a(format!("--pdf-engine={engine}")).args(["-V", "geometry:margin=1in"]);
    } else {
        c = c.args(["-t", pandoc_fmt(s.to, true)]);
        if matches!(s.to, "html" | "tex") {
            c = c.a("-s");
        }
        if s.to == "html" {
            c = c.a("--mathjax");
        }
    }
    vec![c]
}

fn soffice(s: &Step) -> Vec<Cmd> {
    let filter = match s.to {
        "txt" => "txt:Text",
        "csv" => "csv:Text - txt - csv (StarCalc):44,34,76", // comma, quote, UTF-8
        f => f,
    };
    // A private profile lets several conversions (and an open LibreOffice window) coexist.
    let profile = cat("-env:UserInstallation=file://", &s.tmp.join("lo"));
    let out = s.tmp.join(format!("{}.{}", s.stem(), s.to));
    vec![cmd("soffice").a(profile).args(["--headless", "--convert-to", filter, "--outdir"]).a(s.tmp).a(s.src).out(out)]
}

fn sh(script: &str) -> Cmd {
    cmd("sh").args(["-c", script, "sh"])
}

fn edges() -> &'static [Edge] {
    static E: OnceLock<Vec<Edge>> = OnceLock::new();
    E.get_or_init(|| {
        let mut v = vec![
            // documents
            edge(&["markitdown"], &[&["pdf", "docx", "pptx", "xlsx", "xls", "html", "epub"]], &[&["md"]],
                |s| vec![cmd("markitdown").a(s.src).a("-o").a(s.dst)]),
            edge(&["latexmk|tectonic|pdflatex|xelatex"], &[&["tex"]], &[&["pdf"]], latex),
            edge(&["typst"], &[&["typ"]], &[&["pdf"]], |s| vec![cmd("typst").a("compile").a(s.src).a(s.dst)]),
            // Office layouts survive LibreOffice's PDF export; pandoc would reflow them through LaTeX.
            edge(&["soffice"], &[WRITER, CALC, IMPRESS], &[&["pdf"]], soffice),
            edge(&["pdftotext"], &[&["pdf"]], &[&["txt", "md"]], |s| {
                let layout: &[&str] = if s.to == "txt" { &["-layout"] } else { &[] };
                vec![cmd("pdftotext").args(["-enc", "UTF-8"]).args(layout).a(s.src).a(s.dst)]
            }),
            edge(&["pdftohtml"], &[&["pdf"]], &[&["html"]],
                |s| vec![cmd("pdftohtml").args(["-s", "-i", "-noframes", "-q"]).a(s.src).a(s.dst)]),
            edge(&["pdftocairo"], &[&["pdf"]], &[&["svg", "eps", "ps"]], |s| {
                let pages: &[&str] = if s.to == "ps" { &[] } else { &["-f", "1", "-l", "1"] };
                vec![cmd("pdftocairo").a(format!("-{}", s.to)).args(pages).a(s.src).a(s.dst)]
            }),
            // sheets: stdlib python is instant for the plain-data formats
            edge(&["python3"], &[&["xlsx", "csv", "tsv", "json"]], &[&["csv", "tsv", "json", "xlsx"]],
                |s| vec![cmd("python3").args(["-I", "-c", SHEETS_PY]).a(s.src).a(s.dst).args([s.from, s.to])]),
            edge(&["pandoc"], &[PANDOC_IN], &[PANDOC_OUT], pandoc),
            edge(&["pandoc", "tectonic|xelatex|lualatex|pdflatex|typst"], &[PANDOC_IN], &[&["pdf"]], pandoc),
            edge(&["textutil"], &[TEXTUTIL], &[TEXTUTIL],
                |s| vec![cmd("textutil").args(["-convert", s.to, "-output"]).a(s.dst).a(s.src)]),
            edge(&["soffice"], &[WRITER], &[&["docx", "odt", "doc", "rtf", "txt", "html"]], soffice),
            edge(&["soffice"], &[CALC], &[&["xlsx", "ods", "xls", "csv", "html"]], soffice),
            edge(&["soffice"], &[IMPRESS], &[&["pptx", "odp", "ppt"]], soffice),
            edge(&["ebook-convert"], &[&["epub", "mobi", "azw3", "fb2", "docx", "html", "txt"]],
                &[&["epub", "mobi", "azw3", "pdf", "docx", "txt"]], |s| vec![cmd("ebook-convert").a(s.src).a(s.dst)]),
            // images
            edge(&["rsvg-convert"], &[&["svg"]], &[&["png", "pdf", "eps", "ps"]],
                |s| vec![cmd("rsvg-convert").args(["-f", s.to, "-o"]).a(s.dst).a(s.src)]),
            edge(&["sips"], &[IMG, RAW, &["svg"]], &[SIPS_OUT], |s| {
                // ico must be square and at most 256px: fit, then pad.
                let resize: &[&str] = if s.to == "ico" { &["-Z", "256", "--padToHeightWidth", "256", "256"] } else { &[] };
                let fmt = if s.to == "jpg" { "jpeg" } else { s.to };
                vec![cmd("sips").args(["-s", "format", fmt]).args(resize).a(s.src).a("--out").a(s.dst)]
            }),
            edge(&["magick"], &[IMG, RAW, &["svg"]], &[IMG, &["pdf"]], |s| {
                let src = if s.from == "gif" && s.to != "gif" { cat("", s.src).into_string().map(|p| p + "[0]").map_or(s.src.into(), OsString::from) } else { s.src.into() };
                vec![cmd("magick").a(src).a(s.dst)]
            }),
            edge(&["cwebp"], &[&["png", "jpg", "tiff", "webp"]], &[&["webp"]],
                |s| vec![cmd("cwebp").args(["-quiet", "-q", "85"]).a(s.src).a("-o").a(s.dst)]),
            edge(&["dwebp"], &[&["webp"]], &[&["png"]], |s| vec![cmd("dwebp").a("-quiet").a(s.src).a("-o").a(s.dst)]),
            edge(&["ffmpeg"], &[&["png", "jpg", "bmp", "tiff", "gif", "webp"]], &[&["png", "jpg", "bmp", "tiff", "gif"]],
                |s| vec![ff(s).args(["-frames:v", "1", "-update", "1", "-q:v", "2"]).a(s.dst)]),
            // video / audio
            // listed before video->gif so "video -> some image" grabs one frame, not an animation
            edge(&["ffmpeg"], &[VIDEO], &[&["jpg", "png"]],
                |s| vec![ff(s).args(["-vf", "thumbnail", "-frames:v", "1", "-update", "1"]).a(s.dst)]),
            edge(&["ffmpeg"], &[VIDEO, &["gif"]], &[&["mp4", "mov", "mkv", "webm", "avi", "m4v", "gif"]], ff_video),
            edge(&["ffmpeg"], &[AUDIO, VIDEO], &[AUDIO_OUT], ff_audio),
            edge(&["ffmpeg"], &[SUBS, &["mkv", "mp4", "mov", "webm"]], &[SUBS], |s| {
                let map: &[&str] = if SUBS.contains(&s.from) { &[] } else { &["-map", "0:s:0"] };
                vec![ff(s).args(map).a(s.dst)]
            }),
            // archives (bsdtar reads zip/7z/rar/iso and writes by suffix with -a)
            edge(&["tar", "sh"], &[ARCH, &["rar", "iso"]], &[ARCH],
                |s| vec![sh(r#"mkdir -p "$1" && tar -xf "$2" -C "$1" && tar -C "$1" -acf "$3" ."#).a(s.tmp).a(s.src).a(s.dst)]),
            edge(&["tar"], &[&["dir"]], &[ARCH],
                |s| vec![cmd("tar").a("-C").a(s.dir()).a("-acf").a(s.dst).a(s.src.file_name().unwrap_or_default())]),
            // An archive holding one top-level folder becomes that folder, not dst/folder/...
            edge(&["tar", "sh"], &[ARCH, &["rar", "iso"]], &[&["dir"]], |s| vec![sh(concat!(
                r#"mkdir -p "$1" && tar -xf "$2" -C "$1" && one="$1/$(ls -A "$1")" && "#,
                r#"if [ "$(ls -A "$1" | wc -l)" -eq 1 ] && [ -d "$one" ]; then mv "$one" "$3"; else mv "$1" "$3"; fi"#,
            )).a(s.tmp.join("x")).a(s.src).a(s.dst)]),
        ];
        // pdf pages -> images writes notes-1.png, notes-2.png, ... next to the output.
        v.insert(4, Edge { multi: true, ..edge(&["pdftoppm"], &[&["pdf"]], &[&["png", "jpg", "tiff"]], |s| {
            let flag = match s.to { "jpg" => "-jpeg", "tiff" => "-tiff", _ => "-png" };
            vec![cmd("pdftoppm").args([flag, "-r", "300"]).a(s.src).a(s.dst.with_extension(""))]
        }) });
        v
    })
}

// ---------- routing ----------

/// BFS predecessor map: format -> (previous format, edge index).
fn search(from: &str, ok: fn(&Edge) -> bool) -> HashMap<String, (String, usize)> {
    let es = edges();
    let mut prev: HashMap<String, (String, usize)> = HashMap::new();
    let mut seen = HashSet::from([from.to_string()]);
    let mut q = VecDeque::from([from.to_string()]);
    while let Some(f) = q.pop_front() {
        if prev.get(&f).is_some_and(|(_, i)| es[*i].multi) {
            continue;
        }
        for (i, e) in es.iter().enumerate().filter(|(_, e)| ok(e) && e.from.contains(&f.as_str())) {
            for &t in &e.to {
                if seen.insert(t.into()) {
                    prev.insert(t.into(), (f.clone(), i));
                    q.push_back(t.into());
                }
            }
        }
    }
    prev
}

fn route(from: &str, to: &str, ok: fn(&Edge) -> bool) -> Option<Vec<(usize, String)>> {
    let prev = search(from, ok);
    let mut path = vec![];
    let mut cur = to.to_string();
    while cur != from {
        let (p, i) = prev.get(&cur)?;
        path.push((*i, cur));
        cur = p.clone();
    }
    path.reverse();
    Some(path)
}

fn missing(r: &[(usize, String)]) -> String {
    let mut out: Vec<String> = vec![];
    for (i, _) in r {
        for n in edges()[*i].needs.iter().filter(|n| !n.split('|').any(has)) {
            let t = n.split('|').next().unwrap();
            let s = format!("{t} ({})", install_hint(t));
            if !out.contains(&s) {
                out.push(s);
            }
        }
    }
    out.join(", ")
}

// ---------- running ----------

struct Opts {
    force: bool,
    dry: bool,
    out: Option<PathBuf>,
}

struct Target {
    fmt: String,
    ext: String,
}

fn show(p: &Path) -> String {
    let cwd = env::current_dir().unwrap_or_default();
    p.strip_prefix(&cwd).unwrap_or(p).display().to_string()
}

fn tail(tool: &str, o: &Output) -> String {
    let text = format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let keep = &lines[lines.len().saturating_sub(12)..];
    format!("{tool} failed ({})\n    {}", o.status, keep.join("\n    "))
}

fn mv(a: &Path, b: &Path) -> Result<(), String> {
    fs::rename(a, b)
        .or_else(|_| fs::copy(a, b).map(|_| ()).and_then(|_| fs::remove_file(a)))
        .map_err(|e| format!("moving {}: {e}", show(a)))
}

fn run(e: &Edge, s: &Step) -> Result<(), String> {
    exec((e.run)(s), s.dst, e.multi, format!("no installed tool for {} -> {}", s.from, s.to))
}

/// Try installed alternatives in order; the first that exits 0 and writes `dst` wins.
fn exec(alts: Vec<Cmd>, dst: &Path, multi: bool, none: String) -> Result<(), String> {
    let mut err: Option<String> = None;
    for c in alts.into_iter().filter(|c| has(c.tool)) {
        let mut p = Command::new(which(c.tool).unwrap());
        p.args(&c.args).stdin(Stdio::null());
        if let Some(d) = &c.cwd {
            p.current_dir(d);
        }
        let o = p.output().map_err(|x| format!("{}: {x}", c.tool))?;
        let made = c.out.as_deref().unwrap_or(dst);
        if o.status.success() && (made.exists() || multi) {
            if made != dst && made.exists() {
                mv(made, dst)?;
            }
            return Ok(());
        }
        err = err.or(Some(tail(c.tool, &o))); // report the preferred tool's failure
    }
    Err(err.unwrap_or(none))
}

/// Files `stem-N.ext` written by a page-splitting tool next to `dst`.
fn pages(dst: &Path) -> Vec<PathBuf> {
    let stem = format!("{}-", dst.file_stem().unwrap_or_default().to_string_lossy());
    let mut v: Vec<PathBuf> = fs::read_dir(dst.parent().unwrap_or(Path::new(".")))
        .into_iter()
        .flatten()
        .flatten()
        .map(|d| d.path())
        .filter(|p| {
            let n = p.file_stem().unwrap_or_default().to_string_lossy();
            n.strip_prefix(&stem).is_some_and(|k| !k.is_empty() && k.bytes().all(|b| b.is_ascii_digit()))
        })
        .collect();
    v.sort();
    v
}

fn plan(src: &Path, to: &str) -> Result<(String, String, Vec<(usize, String)>), String> {
    let (from, stem) = detect(src);
    if from.is_empty() {
        return Err("unknown input type (no extension)".into());
    }
    if from == to {
        return Err(format!("already {from}"));
    }
    if !relevant(&from, to) {
        return Err(format!("{from} -> {to} isn't a sensible conversion"));
    }
    let r = route(&from, to, Edge::usable).ok_or_else(|| match route(&from, to, |_| true) {
        Some(r) => format!("needs {}", missing(&r)),
        None => format!("no known route {from} -> {to}"),
    })?;
    Ok((from, stem, r))
}

fn via(r: &[(usize, String)]) -> String {
    r.iter().map(|(i, _)| edges()[*i].name()).collect::<Vec<_>>().join(" -> ")
}

fn convert(src: &Path, t: &Target, o: &Opts, id: usize) -> Result<String, String> {
    let (from, stem, r) = plan(src, &t.fmt)?;
    let outdir = o.out.clone().unwrap_or_else(|| src.parent().unwrap_or(Path::new(".")).into());
    let dst = outdir.join(if t.fmt == "dir" { stem.clone() } else { format!("{stem}.{}", t.ext) });
    let via = via(&r);
    if o.dry {
        let hops: Vec<&str> = r.iter().map(|(_, f)| f.as_str()).collect();
        return Ok(format!("{} -> {}  [{from} -> {}  via {via}]", show(src), show(&dst), hops.join(" -> ")));
    }
    if dst.exists() && !o.force {
        return Err(format!("{} exists (use -f to overwrite)", show(&dst)));
    }
    execute(src, &from, &stem, &r, &dst, id)?;

    let mut wrote = show(&dst);
    if !dst.exists() {
        let p = pages(&dst);
        match p.as_slice() {
            [] => return Err("tool reported success but wrote nothing".into()),
            [one] => mv(one, &dst)?,
            [first, .., last] => wrote = format!("{} .. {} ({} pages)", show(first), show(last), p.len()),
        }
    }
    Ok(format!("{} -> {wrote}  ({via})", show(src)))
}

/// Run route `r` from `src` to `dst`, with intermediates in a private temp dir.
fn execute(src: &Path, from: &str, stem: &str, r: &[(usize, String)], dst: &Path, id: usize) -> Result<(), String> {
    let tmp = env::temp_dir().join(format!("conv-{}-{id}", std::process::id()));
    let result = (|| {
        let (mut cur, mut cur_fmt) = (src.to_path_buf(), from.to_string());
        for (k, (i, f)) in r.iter().enumerate() {
            let next = if k + 1 == r.len() {
                dst.to_path_buf()
            } else {
                tmp.join(if f == "dir" { stem.into() } else { format!("{stem}.{f}") })
            };
            let scratch = tmp.join(format!(".step{k}"));
            fs::create_dir_all(&scratch).map_err(|e| e.to_string())?;
            let step = Step { src: &cur, dst: &next, from: &cur_fmt, to: f, tmp: &scratch };
            run(&edges()[*i], &step)?;
            (cur, cur_fmt) = (next, f.clone());
        }
        Ok::<(), String>(())
    })();
    let _ = fs::remove_dir_all(&tmp);
    if result.is_err() && dst.is_file() {
        let _ = fs::remove_file(dst); // don't leave a half-written output behind
    }
    result
}

/// Concatenate inputs in argument order into one pdf, converting non-pdf inputs first.
fn join(inputs: &[PathBuf], out: &Path, o: &Opts) -> Result<String, String> {
    if canon(&out.extension().unwrap_or_default().to_string_lossy()) != "pdf" {
        return Err("-j currently joins into pdf only".into());
    }
    if out.exists() && !o.force {
        return Err(format!("{} exists (use -f to overwrite)", show(out)));
    }
    let tmp = env::temp_dir().join(format!("conv-{}-join", std::process::id()));
    fs::create_dir_all(&tmp).map_err(|e| e.to_string())?;
    let parts: Vec<Result<PathBuf, String>> = thread::scope(|sc| {
        let hs: Vec<_> = inputs.iter().enumerate().map(|(k, p)| {
            let part = tmp.join(format!("{k}.pdf"));
            sc.spawn(move || {
                if detect(p).0 == "pdf" {
                    return Ok(p.clone());
                }
                let (from, stem, r) = plan(p, "pdf")?;
                execute(p, &from, &stem, &r, &part, k).map(|_| part)
            })
        }).collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let result = (|| {
        let mut ok = vec![];
        for (p, part) in inputs.iter().zip(parts) {
            ok.push(part.map_err(|e| format!("{}: {e}", show(p)))?);
        }
        exec(vec![
            cmd("pdfunite").args(&ok).a(out),
            cmd("qpdf").a("--empty").a("--pages").args(&ok).a("--").a(out),
            cmd("gs").args(["-q", "-dBATCH", "-dNOPAUSE", "-sDEVICE=pdfwrite"]).a(cat("-sOutputFile=", out)).args(&ok),
        ], out, false, "join needs pdfunite (brew install poppler), qpdf or gs".into())
    })();
    let _ = fs::remove_dir_all(&tmp);
    result.map(|_| format!("{} inputs -> {}", inputs.len(), show(out)))
}

fn list(p: &Path) {
    let (from, _) = detect(p);
    let mut have = search(&from, Edge::usable);
    have.retain(|f, _| relevant(&from, f) && route(&from, f, Edge::usable).is_some_and(|r| r.len() <= 2));
    let mut fmts: Vec<&String> = have.keys().collect();
    fmts.sort();
    let fmts: Vec<&str> = fmts.into_iter().map(String::as_str).collect();
    println!("{} ({from}) -> {}", show(p), if fmts.is_empty() { "nothing with installed tools".into() } else { fmts.join(" ") });
    let mut more: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for f in search(&from, |_| true).into_keys().filter(|f| !have.contains_key(f) && relevant(&from, f)) {
        if let Some(r) = route(&from, &f, |_| true).filter(|r| r.len() <= 2) {
            more.entry(missing(&r)).or_default().push(f);
        }
    }
    for (need, mut fs) in more {
        fs.sort();
        println!("  + {}  with {need}", fs.join(" "));
    }
}

fn main() {
    let mut o = Opts { force: false, dry: false, out: None };
    let mut joined: Option<PathBuf> = None;
    let mut pos = vec![];
    let mut args = env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "-f" | "--force" => o.force = true,
            "-n" | "--dry-run" => o.dry = true,
            "-o" | "--out" => o.out = args.next().map(PathBuf::from),
            "-j" | "--join" => joined = args.next().map(PathBuf::from),
            "-h" | "--help" => return print!("{USAGE}"),
            _ => pos.push(a),
        }
    }

    // Trailing arguments that name an output format (and aren't files) are targets.
    let outs: HashSet<&str> = edges().iter().flat_map(|e| e.to.iter().copied()).chain(["dir"]).collect();
    let mut targets = vec![];
    while pos.len() > 1 && joined.is_none() {
        let a = pos.last().unwrap();
        let f = canon(a);
        if !outs.contains(f.as_str()) || Path::new(a).exists() {
            break;
        }
        targets.push(Target { fmt: f, ext: a.trim_start_matches('.').to_lowercase() });
        pos.pop();
    }
    targets.reverse();
    if pos.is_empty() {
        eprint!("{USAGE}");
        exit(2);
    }

    let mut failed: usize = 0;
    let mut inputs = vec![];
    for a in &pos {
        match fs::canonicalize(a) {
            Ok(p) => inputs.push(p),
            Err(e) => {
                eprintln!("x {a}: {e}");
                failed += 1;
            }
        }
    }
    if let Some(out) = joined {
        match join(&inputs, &out, &o) {
            Ok(m) => println!("ok {m}"),
            Err(e) => {
                eprintln!("x join: {e}");
                failed += 1;
            }
        }
        exit(failed.min(1) as i32);
    }
    if targets.is_empty() {
        inputs.iter().for_each(|p| list(p));
        exit(failed.min(1) as i32);
    }
    if let Some(d) = o.out.take() {
        match fs::create_dir_all(&d).and_then(|_| fs::canonicalize(&d)) {
            Ok(d) => o.out = Some(d),
            Err(e) => {
                eprintln!("x -o {}: {e}", d.display());
                exit(1);
            }
        }
    }

    let jobs: Vec<(&PathBuf, &Target)> = inputs.iter().flat_map(|p| targets.iter().map(move |t| (p, t))).collect();
    let next = AtomicUsize::new(0);
    let fails = AtomicUsize::new(failed);
    let workers = thread::available_parallelism().map_or(4, |n| n.get()).min(jobs.len());
    thread::scope(|sc| {
        for _ in 0..workers {
            sc.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some((p, t)) = jobs.get(i) else { break };
                let t0 = Instant::now();
                match convert(p, t, &o, i) {
                    Ok(m) if o.dry => println!("{m}"),
                    Ok(m) => println!("ok {m}  {:.2}s", t0.elapsed().as_secs_f64()),
                    Err(e) => {
                        eprintln!("x {} -> {}: {e}", show(p), t.ext);
                        fails.fetch_add(1, Ordering::Relaxed);
                    }
                }
            });
        }
    });
    exit((fails.into_inner() > 0) as i32);
}

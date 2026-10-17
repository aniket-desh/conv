<h1><img src="assets/logo.svg" width="40" align="center" alt="">&nbsp;conv</h1>

**Convert anything. Right from your terminal.**

One command and any file becomes the format you need. It runs locally and in parallel, and it picks the best tool you already have installed.

```sh
conv notes.pdf md
```

That's the whole interface: a file, then the format(s) you want.

---

## ✨ The basics

```sh
conv notes.pdf md                 # → notes.md
conv notes.tex pdf md html        # three outputs at once, built in parallel
conv *.heic jpg                   # a whole folder of iPhone photos
conv talk.mov mp4 gif mp3         # remux, animate, rip the audio
conv photos zip                   # folders ↔ archives
```

Outputs land next to the input. Add `-o DIR` to put them somewhere else, or `-f` to overwrite.

## 🪄 Tricks

**Ask what's possible.** Give it a file with no target to see everything that file can become, plus what to install to unlock more.

```console
$ conv data.csv
data.csv (csv) -> docx epub html json md odt pdf rst tex tsv txt xlsx …
  + ods xls  with soffice (brew install --cask libreoffice)
```

**Compile LaTeX.** `.tex` files go through `latexmk` (xelatex if you use `fontspec`). They also convert straight to Markdown, HTML or Word.

```sh
conv thesis.tex pdf               # compiled, with refs and bibliography
conv thesis.tex png               # tex → pdf → one png per page
conv thesis.tex md                # math stays as $…$
```

**Stitch things together.** `-j` joins inputs in the order you list them, converting anything that isn't already a PDF.

```sh
conv cover.heic notes.docx appendix.pdf -j packet.pdf
```

**See the route.** `-n` shows the plan without running it.

```console
$ conv -n notes.tex png
notes.tex -> notes.png  [tex -> pdf -> png  via latexmk -> pdftoppm]
```

## 🗂️ What it converts

| | Formats | Notes |
|---|---|---|
| 🖼️ **Images** | `jpg` `png` `heic` `avif` `webp` `tiff` `gif` `bmp` `ico` `icns` `psd` `jp2` `tga` `exr` `jxl` `svg` + RAW (`dng` `cr2` `cr3` `nef` `arw` `raf` …) | macOS-native `sips`, so HEIC just works |
| 🎬 **Video** | `mp4` `mov` `mkv` `webm` `avi` `m4v` `wmv` `flv` `mpg` `ts` `3gp` `gif` | stream-copies when codecs fit (instant), otherwise re-encodes; palette-optimized GIFs |
| 🎧 **Audio** | `mp3` `m4a` `aac` `wav` `flac` `ogg` `opus` `aiff` `wma` `caf` | pulls the audio out of any video too |
| 📄 **PDF** | → `txt` `md` `html` `docx` `svg` `eps` `png` `jpg` `tiff` | every page at 300 dpi |
| 📝 **Documents** | `md` `tex` `html` `docx` `doc` `odt` `rtf` `rtfd` `epub` `rst` `org` `ipynb` `typ` `txt` `pages` | via pandoc, textutil, LibreOffice |
| 📊 **Sheets** | `xlsx` `csv` `tsv` `json` `xls` `ods` `numbers` | xlsx/csv/tsv/json need nothing extra |
| 📽️ **Slides** | `pptx` `ppt` `key` `odp` → `pdf` `png` `md` | and `md` → `pptx` |
| 💬 **Subtitles** | `srt` `vtt` `ass` | can also extract subtitles from `mkv`/`mp4` |
| 📚 **Ebooks** | `epub` `mobi` `azw3` `fb2` | mobi/azw3 via calibre |
| 📦 **Archives** | `zip` `tar` `tgz` `txz` `tbz2` `7z`, plus `rar` `iso` in | folders zip up, archives unpack to folders |

## 📥 Install

```sh
git clone https://github.com/aniket-desh/conv && cd conv
cargo install --path .
```

conv drives the converters already on your machine. These cover almost everything:

```sh
brew install ffmpeg pandoc poppler librsvg webp
```

### Supercharge (optional)

| Install | Unlocks |
|---|---|
| `brew install --cask libreoffice` | layout-faithful `docx`/`pptx`/`xlsx` → `pdf`, plus `doc` `xls` `ods` `key` `pages` `numbers` |
| `brew install imagemagick` | `jxl` and long-tail image formats |
| `brew install --cask mactex-no-gui` or `brew install tectonic` | compiling `.tex` → `pdf` |
| `pipx install markitdown` | better `pdf`/`docx`/`pptx` → `md` |
| `brew install --cask calibre` | `mobi` `azw3` |

Don't remember what you have? Just run `conv yourfile` and it'll tell you.

## ⚙️ How it works

conv is a small Rust router with no dependencies. It starts in about 1 ms, and none of the conversion happens in its own code. Each edge in its table says *"tool T turns formats A into B"*. For each request, a breadth-first search finds the shortest chain of tools you actually have:

```
heic ──sips──▶ png ──cwebp──▶ webp
tex ──latexmk──▶ pdf ──pdftoppm──▶ png
```

Ties go to the edge listed first, so the table also encodes preference: native tools before generic ones, stream copy before re-encode, LibreOffice before pandoc for office → pdf. Jobs (every input × every target) run across all your cores. Intermediate files live in a private temp dir, and a failed run never leaves a half-written output behind.

Adding a format usually means adding one line to the table in [`src/main.rs`](src/main.rs).

## Honest limits

- `pdf → md` extracts text, so prose is fine but equations come out mangled. Install markitdown for better results.
- Without LibreOffice, `docx`/`pptx` → `pdf` reflows the content through LaTeX instead of keeping the original layout.
- The built-in `xlsx` reader takes the first sheet and leaves dates as Excel serial numbers.
- `-j` joins into PDF only, for now.

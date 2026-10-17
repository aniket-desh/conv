# Spreadsheet fallback used when LibreOffice is absent: xlsx/csv/tsv/json, stdlib only.
# usage: python3 -c <this> SRC DST FROM TO   (xlsx reads/writes the first sheet only)
import csv, json, re, sys, zipfile
from xml.etree import ElementTree as ET
from xml.sax.saxutils import escape

src, dst, fi, fo = sys.argv[1:5]
M = "http://schemas.openxmlformats.org/spreadsheetml/2006/main"
R = "http://schemas.openxmlformats.org/officeDocument/2006/relationships"
NUM = re.compile(r"-?\d+(\.\d+)?([eE][-+]?\d+)?")


def col(ref):
    n = 0
    for ch in re.match(r"[A-Z]+", ref).group():
        n = n * 26 + ord(ch) - 64
    return n - 1


def colname(i):
    s = ""
    i += 1
    while i:
        i, r = divmod(i - 1, 26)
        s = chr(65 + r) + s
    return s


def read_xlsx(p):
    z, m = zipfile.ZipFile(p), "{%s}" % M
    ss = []
    if "xl/sharedStrings.xml" in z.namelist():
        root = ET.fromstring(z.read("xl/sharedStrings.xml"))
        ss = ["".join(t.text or "" for t in si.iter(m + "t")) for si in root.iter(m + "si")]
    rid = ET.fromstring(z.read("xl/workbook.xml")).find(f"{m}sheets/{m}sheet").get(f"{{{R}}}id")
    rels = ET.fromstring(z.read("xl/_rels/workbook.xml.rels"))
    tgt = next(r.get("Target") for r in rels if r.get("Id") == rid)
    tgt = tgt.lstrip("/") if tgt.startswith("/") else "xl/" + tgt
    rows = []
    for r in ET.fromstring(z.read(tgt)).iter(m + "row"):
        rows += [[] for _ in range(int(r.get("r", len(rows) + 1)) - 1 - len(rows))]
        row = []
        for c in r.iter(m + "c"):
            i = col(c.get("r")) if c.get("r") else len(row)
            t, v = c.get("t"), c.find(m + "v")
            if t == "s":
                x = ss[int(v.text)]
            elif t == "inlineStr":
                x = "".join(e.text or "" for e in c.iter(m + "t"))
            elif t == "b":
                x = "TRUE" if v is not None and v.text == "1" else "FALSE"
            else:
                x = v.text if v is not None and v.text else ""
            row += [""] * (i - len(row))
            row.append(x)
        rows.append(row)
    return rows


def write_xlsx(p, rows):
    strings = {}  # shared-string table: what Excel writes, and what every reader understands

    def cell(r, i, x):
        ref = f"{colname(i)}{r}"
        if NUM.fullmatch(x):
            return f'<c r="{ref}"><v>{x}</v></c>'
        return f'<c r="{ref}" t="s"><v>{strings.setdefault(x, len(strings))}</v></c>'

    data = "".join(
        f'<row r="{r}">' + "".join(cell(r, i, x) for i, x in enumerate(row) if x != "") + "</row>"
        for r, row in enumerate(rows, 1)
    )
    ct = "application/vnd.openxmlformats-officedocument.spreadsheetml"
    rel = "http://schemas.openxmlformats.org/package/2006/relationships"
    with zipfile.ZipFile(p, "w", zipfile.ZIP_DEFLATED) as z:
        z.writestr("[Content_Types].xml",
            '<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">'
            f'<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>'
            '<Default Extension="xml" ContentType="application/xml"/>'
            f'<Override PartName="/xl/workbook.xml" ContentType="{ct}.sheet.main+xml"/>'
            f'<Override PartName="/xl/worksheets/sheet1.xml" ContentType="{ct}.worksheet+xml"/>'
            f'<Override PartName="/xl/sharedStrings.xml" ContentType="{ct}.sharedStrings+xml"/></Types>')
        z.writestr("_rels/.rels", f'<Relationships xmlns="{rel}"><Relationship Id="rId1" '
            f'Type="{R}/officeDocument" Target="xl/workbook.xml"/></Relationships>')
        z.writestr("xl/workbook.xml", f'<workbook xmlns="{M}" xmlns:r="{R}"><sheets>'
            '<sheet name="Sheet1" sheetId="1" r:id="rId1"/></sheets></workbook>')
        z.writestr("xl/_rels/workbook.xml.rels", f'<Relationships xmlns="{rel}"><Relationship Id="rId1" '
            f'Type="{R}/worksheet" Target="worksheets/sheet1.xml"/><Relationship Id="rId2" '
            f'Type="{R}/sharedStrings" Target="sharedStrings.xml"/></Relationships>')
        z.writestr("xl/worksheets/sheet1.xml", f'<worksheet xmlns="{M}"><sheetData>{data}</sheetData></worksheet>')
        si = "".join(f'<si><t xml:space="preserve">{escape(x)}</t></si>' for x in strings)
        z.writestr("xl/sharedStrings.xml", f'<sst xmlns="{M}" count="{len(strings)}" uniqueCount="{len(strings)}">{si}</sst>')


def read_json(p):
    data = json.load(open(p, encoding="utf-8"))
    s = lambda v: "" if v is None else v if isinstance(v, str) else json.dumps(v)
    if data and all(isinstance(d, dict) for d in data):
        keys = list(dict.fromkeys(k for d in data for k in d))
        return [keys] + [[s(d.get(k)) for k in keys] for d in data]
    return [[s(v) for v in row] for row in data]


def write_json(p, rows):
    num = lambda x: (float(x) if any(c in x for c in ".eE") else int(x)) if NUM.fullmatch(x) else x
    head, body = (rows[0], rows[1:]) if rows else ([], [])
    out = [{k: num(row[i]) if i < len(row) else "" for i, k in enumerate(head)} for row in body]
    json.dump(out, open(p, "w", encoding="utf-8"), indent=2, ensure_ascii=False)


delim = {"csv": ",", "tsv": "\t"}
if fi == "xlsx":
    rows = read_xlsx(src)
elif fi == "json":
    rows = read_json(src)
else:
    rows = list(csv.reader(open(src, encoding="utf-8-sig", newline=""), delimiter=delim[fi]))

if fo == "xlsx":
    write_xlsx(dst, rows)
elif fo == "json":
    write_json(dst, rows)
else:
    csv.writer(open(dst, "w", encoding="utf-8", newline=""), delimiter=delim[fo], lineterminator="\n").writerows(rows)

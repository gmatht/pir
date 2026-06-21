#![cfg(debug_assertions)]

use std::fs;
use std::io::{ErrorKind, Write};
use std::process::{Command, Stdio};

use corro::formula::{cell_effective_display, refresh_spills};
use corro::grid::{CellAddr, Grid};

fn libreoffice_bin() -> Option<String> {
    for candidate in ["libreoffice", "soffice"] {
        if Command::new(candidate)
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
        {
            return Some(candidate.to_string());
        }
    }
    None
}

fn write_sample_xlsx(path: &std::path::Path) {
    let file = fs::File::create(path).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    let opt = zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Stored);

    zip.start_file("[Content_Types].xml", opt).unwrap();
    zip.write_all(
        br#"<?xml version="1.0" encoding="UTF-8"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.workbook+xml"/>
  <Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>
</Types>"#,
    )
    .unwrap();

    zip.add_directory("_rels/", opt).unwrap();
    zip.start_file("_rels/.rels", opt).unwrap();
    zip.write_all(
        br#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/>
</Relationships>"#,
    )
    .unwrap();

    zip.add_directory("xl/_rels/", opt).unwrap();
    zip.start_file("xl/_rels/workbook.xml.rels", opt).unwrap();
    zip.write_all(
        br#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/>
</Relationships>"#,
    )
    .unwrap();

    zip.start_file("xl/workbook.xml", opt).unwrap();
    zip.write_all(
        br#"<?xml version="1.0" encoding="UTF-8"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
  <sheets>
    <sheet name="Sheet1" sheetId="1" r:id="rId1"/>
  </sheets>
</workbook>"#,
    )
    .unwrap();

    zip.add_directory("xl/worksheets/", opt).unwrap();
    zip.start_file("xl/worksheets/sheet1.xml", opt).unwrap();
    zip.write_all(
        br#"<?xml version="1.0" encoding="UTF-8"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <sheetData>
    <row r="1">
      <c r="A1" t="n"><v>1</v></c>
      <c r="B1" t="n"><v>2</v></c>
      <c r="C1"><f>SUM(A1:B1)</f><v>3</v></c>
    </row>
  </sheetData>
</worksheet>"#,
    )
    .unwrap();

    zip.finish().unwrap();
}

fn tsv_matrix(s: &str) -> Vec<Vec<String>> {
    s.lines()
        .map(|line| line.split('\t').map(|v| v.to_string()).collect())
        .collect()
}

#[test]
fn libreoffice_smoke_roundtrip() {
    let Some(bin) = libreoffice_bin() else {
        eprintln!("LibreOffice not installed; skipping harness");
        return;
    };

    let dir = tempfile::tempdir().unwrap();
    let xlsx = dir.path().join("sample.xlsx");
    write_sample_xlsx(&xlsx);

    let outdir = dir.path().join("lo");
    fs::create_dir(&outdir).unwrap();

    let status = match Command::new(&bin)
        .arg("--headless")
        .arg("--convert-to")
        .arg("tsv")
        .arg("--outdir")
        .arg(&outdir)
        .arg(&xlsx)
        .status()
    {
        Ok(status) => status,
        Err(err) if err.kind() == ErrorKind::NotFound => {
            eprintln!("LibreOffice not found; skipping harness");
            return;
        }
        Err(err) => panic!("failed to run LibreOffice: {err}"),
    };
    assert!(status.success());

    let lo_tsv = fs::read_to_string(outdir.join("sample.tsv")).unwrap();
    let lo = tsv_matrix(&lo_tsv);

    let mut concrete = Grid::new(1, 3);
    concrete.set(&CellAddr::Main { row: 0, col: 0 }, "1".into());
    concrete.set(&CellAddr::Main { row: 0, col: 1 }, "2".into());
    concrete.set(&CellAddr::Main { row: 0, col: 2 }, "=SUM(A1:B1)".into());

    // Box the concrete grid to match APIs that expect the boxed Grid alias.
    let mut g = corro::grid::GridBox::from(concrete);

    refresh_spills(&mut g);
    let corro = vec![vec![
        cell_effective_display(&g, &CellAddr::Main { row: 0, col: 0 }),
        cell_effective_display(&g, &CellAddr::Main { row: 0, col: 1 }),
        cell_effective_display(&g, &CellAddr::Main { row: 0, col: 2 }),
    ]];

    assert_eq!(corro, lo);
}

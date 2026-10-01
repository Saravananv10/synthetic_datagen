//! Ground-truth documents and their "scan text".
//!
//! For every upload the scenario defines what is printed on the document
//! (`fields`). `render` turns those fields into the raw text layer an OCR engine
//! would produce (labels, line breaks, some Devanagari boilerplate). In `llm`
//! vision mode GPT reads that text and re-extracts the fields; in
//! `deterministic` mode the fields are used as-is. Either way the middleware in
//! `backend.rs` validates the extraction and produces the summary the stage
//! agent sees — the same OCR -> JSON -> summary path as the demo.
//!
//! No document *images* are generated: agents only ever see text, and forged-looking
//! ID/payslip images add risk without adding signal.

use serde_json::{json, Value};

use crate::scenario::{Payslip, Scenario};

pub struct Doc {
    pub kind: &'static str,
    /// Demo sample image used as the attachment placeholder in the trace.
    pub fixture: String,
    pub fields: Value,
}

const MONTHS: [&str; 12] = ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];

fn month_label(m: &str) -> String {
    match m.split_once('-') {
        Some((y, mo)) => format!("{} {}", MONTHS[(mo.parse::<usize>().unwrap_or(1).max(1) - 1).min(11)], y),
        None => m.to_string(),
    }
}

fn spaced_aadhaar(a: &str) -> String {
    let d: String = a.chars().filter(|c| c.is_ascii_digit()).collect();
    d.as_bytes().chunks(4).map(|c| String::from_utf8_lossy(c).to_string()).collect::<Vec<_>>().join(" ")
}

fn father_of(sc: &Scenario) -> String {
    if !sc.customer.father.is_empty() {
        return sc.customer.father.clone();
    }
    let last = sc.customer.name.split_whitespace().last().unwrap_or("Kumar");
    format!("Ramesh {last}")
}

// ---- documents the human uploads -------------------------------------------------

pub fn identity_docs(sc: &Scenario, attempt: u32) -> Vec<Doc> {
    let c = &sc.customer;
    let noisy = sc.flags.ocr_noise_first && attempt == 0;
    let mut front_no = spaced_aadhaar(&c.aadhaar);
    let mut pan_no = c.pan.clone();
    if noisy {
        // a misread digit on the Aadhaar front (back stays right) and a letter read as a zero in the PAN
        let last = front_no.pop().and_then(|ch| ch.to_digit(10)).unwrap_or(0);
        front_no.push(char::from_digit((last + 3) % 10, 10).unwrap());
        pan_no.replace_range(0..1, "0");
    }
    let mut docs = vec![
        Doc { kind: "aadhaar_front", fixture: "aadhaar_front".into(), fields: json!({"name": c.name, "dob": c.dob, "aadhaar_number": front_no}) },
        Doc { kind: "aadhaar_back", fixture: "aadhaar_back".into(), fields: json!({"address": c.address, "pincode": c.pincode, "aadhaar_number": spaced_aadhaar(&c.aadhaar)}) },
    ];
    if !(sc.flags.identity_upload_incomplete_first && attempt == 0) {
        docs.insert(0, Doc { kind: "pan", fixture: "pan_card".into(), fields: json!({"pan_number": pan_no, "name": c.name.to_uppercase(), "father_name": father_of(sc).to_uppercase(), "dob": c.dob}) });
    }
    docs
}

pub fn address_proof_docs(sc: &Scenario) -> Vec<Doc> {
    let c = &sc.customer;
    vec![Doc { kind: "address_proof", fixture: "aadhaar_back".into(), fields: json!({"document_type": "electricity bill", "name": c.name, "address": c.address, "pincode": c.pincode}) }]
}

/// Photo + certificate for the `idx`-th ornament. `attempt` counts uploads of gold so far.
pub fn gold_docs(sc: &Scenario, idx: usize, attempt: u32) -> Option<Vec<Doc>> {
    let g = sc.gold.items.get(idx)?;
    let reading = if sc.flags.gold_weight_mismatch_first && attempt == 0 && idx == 0 { g.gross_wt + 4.3 } else { g.scale_reading };
    let net = (g.gross_wt - g.stone_deduction) * 100.0;
    let photo = if g.photo_fixture.is_empty() { "necklace_on_scale" } else { &g.photo_fixture };
    let cert = if g.doc_fixture.is_empty() { "gold_valuation_certificate" } else { &g.doc_fixture };
    Some(vec![
        Doc { kind: "gold_photo", fixture: photo.into(), fields: json!({"ornament_type": g.kind, "scale_reading_g": (reading * 100.0).round() / 100.0}) },
        Doc {
            kind: "gold_certificate",
            fixture: cert.into(),
            fields: json!({
                "certificate_no": format!("GVC-2026-{:04}", 1000 + (sc.seed % 9000)),
                "customer_name": sc.customer.name,
                "ornament_type": g.kind,
                "item_description": g.description,
                "gross_weight_g": g.gross_wt,
                "stone_deduction_g": g.stone_deduction,
                "net_weight_g": net.round() / 100.0,
                "purity_karat": g.karat,
            }),
        },
    ])
}

pub fn payslip_docs(sc: &Scenario, set: &[Payslip]) -> Vec<Doc> {
    let mut sorted: Vec<&Payslip> = set.iter().collect();
    sorted.sort_by(|a, b| a.month.cmp(&b.month));
    sorted
        .iter()
        .map(|p| Doc {
            kind: "payslip",
            fixture: p.fixture.clone(),
            fields: json!({"employee_name": p.name, "employer": sc.income.employer, "month": p.month, "net_pay": p.net_pay}),
        })
        .collect()
}

// ---- scan text -------------------------------------------------------------------

/// What the OCR text layer of this document looks like.
pub fn render(doc: &Doc, sc: &Scenario) -> String {
    let f = &doc.fields;
    let s = |k: &str| f.get(k).map(|v| match v { Value::String(x) => x.clone(), other => other.to_string() }).unwrap_or_default();
    match doc.kind {
        "aadhaar_front" => format!(
            "भारत सरकार\nGovernment of India\n{name}\nजन्म तिथि / DOB: {dob}\n{no}\nमेरा आधार, मेरी पहचान",
            name = s("name"), dob = s("dob"), no = s("aadhaar_number")
        ),
        "aadhaar_back" => format!(
            "भारतीय विशिष्ट पहचान प्राधिकरण\nUnique Identification Authority of India\nAddress: {addr}\n{no}\n1947  help@uidai.gov.in  www.uidai.gov.in",
            addr = s("address"), no = s("aadhaar_number")
        ),
        "pan" => format!(
            "आयकर विभाग  भारत सरकार\nINCOME TAX DEPARTMENT  GOVT. OF INDIA\nस्थायी लेखा संख्या कार्ड\nPermanent Account Number Card\n{pan}\nनाम / Name\n{name}\nपिता का नाम / Father's Name\n{father}\nजन्म की तारीख / Date of Birth\n{dob}\nहस्ताक्षर / Signature",
            pan = s("pan_number"), name = s("name"), father = s("father_name"), dob = s("dob")
        ),
        "address_proof" => format!(
            "ELECTRICITY BILL\nConsumer Name: {name}\nService Address: {addr}\nPIN: {pin}\nBill Date: 03-09-2026",
            name = s("name"), addr = s("address"), pin = s("pincode")
        ),
        "gold_photo" => format!(
            "[PHOTO] A {kind} lying on a stainless-steel digital weighing scale. The scale's LCD display reads {w} g.",
            kind = s("ornament_type"), w = s("scale_reading_g")
        ),
        "gold_certificate" => format!(
            "CERTIFIED GOLD VALUATION CERTIFICATE\nEmpanelled Appraiser - Sample Gold Assay Lab\nCertificate No.: {no}\nDate: 08-09-2026\nAppraiser ID: APP-4491\nCUSTOMER DETAILS\nCustomer Name: {cust}\nITEM DETAILS\nOrnament Type: {kind}\nItem Description: {desc}\nGross Weight: {g} grams\nStone / Dust Deduction: {d} grams\nNet Weight: {n} grams\nPurity / Caratage: {k}K ({k} karat)\nTesting Method: XRF\nNumber of Items: 1\nFine Weight Note: Fine Weight = Net x (K/22)\nVALUATION REMARKS\nEligible jewellery collateral - not bullion\nSignature of Appraiser",
            no = s("certificate_no"), cust = s("customer_name"), kind = s("ornament_type"), desc = s("item_description"),
            g = s("gross_weight_g"), d = s("stone_deduction_g"), n = s("net_weight_g"), k = s("purity_karat")
        ),
        "payslip" => {
            let net = f.get("net_pay").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let ded = (net * 0.043 / 100.0).round() * 100.0;
            let gross = net + ded;
            let basic = (gross * 0.588 / 100.0).round() * 100.0;
            let incentive = (gross * 0.294 / 100.0).round() * 100.0;
            let hra = (gross * 0.078 / 100.0).round() * 100.0;
            let meal = gross - basic - incentive - hra;
            let (pf, pt) = ((ded * 0.57 / 100.0).round() * 100.0, (ded * 0.24 / 100.0).round() * 100.0);
            let health = ded - pf - pt;
            format!(
                "Payslip {month}\n{employer}\nDate of Joining : 2018-06-23   Employee Name : {name}\nPay Period : {month}   Designation : Employee\nWorked Days : 24   Department : Operations\nPAN Number : {pan}\nEarnings | Amount | Deductions | Amount\nBasic Pay | {basic} | Provident Fund | {pf}\nIncentive Pay | {inc} | Professional Tax | {pt}\nHouse Rent Allowance | {hra} | Health Insurance | {health}\nMeal Allowance | {meal}\nTotal Earnings | {gross} | Total Deductions | {ded}\nNet Pay | {net}\nThis is system generated payslip",
                month = month_label(&s("month")), employer = s("employer"), name = s("employee_name"), pan = sc.customer.pan,
                basic = basic, inc = incentive, hra = hra, meal = meal, pf = pf, pt = pt, health = health, gross = gross, ded = ded, net = net
            )
        }
        _ => f.to_string(),
    }
}

/// The JSON keys the vision model must return for each document kind.
pub fn schema_for(kind: &str) -> &'static str {
    match kind {
        "aadhaar_front" => "name, dob (DD/MM/YYYY), aadhaar_number (12 digits, spaces allowed)",
        "aadhaar_back" => "address, pincode (6 digits), aadhaar_number (12 digits, spaces allowed)",
        "pan" => "pan_number, name, father_name, dob (DD/MM/YYYY)",
        "address_proof" => "document_type, name, address, pincode (6 digits)",
        "gold_photo" => "ornament_type (one word, lowercase, e.g. necklace), scale_reading_g (number)",
        "gold_certificate" => "certificate_no, customer_name, ornament_type (one word, lowercase), item_description, gross_weight_g (number), stone_deduction_g (number), net_weight_g (number), purity_karat (number)",
        "payslip" => "employee_name, employer, month (format YYYY-MM), net_pay (number, rupees)",
        _ => "",
    }
}

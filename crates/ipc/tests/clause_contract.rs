use ipc::clause::*;
use serde::Deserialize;
use serde_json::Value;

fn fixture() -> Value {
    serde_json::from_str(include_str!(
        "../../../docs/design/clause-navigation-p2/wire-fixtures.json"
    ))
    .unwrap()
}

#[test]
fn shared_scalar_coordinates_preserve_normalization_and_utf16_units() {
    let data: Value = serde_json::from_str(include_str!(
        "../../../docs/design/clause-navigation-p2/state-fixtures.json"
    ))
    .unwrap();
    for case in data["normalization"].as_array().unwrap() {
        let normalized = normalize_reading(case["input"].as_str().unwrap());
        assert_eq!(
            normalized,
            case["normalized"].as_str().unwrap(),
            "{}",
            case["id"]
        );
        assert_eq!(normalize_reading(&normalized), normalized);
        let expected: Vec<ReadingPosition> =
            serde_json::from_value(case["boundaries"].clone()).unwrap();
        assert_eq!(legal_boundaries(&normalized).unwrap(), expected);
        assert_eq!(
            display_end(&normalized).unwrap().0 as u64,
            case["utf16"].as_u64().unwrap()
        );
    }
    assert_eq!(
        legal_boundaries("\u{3099}\u{309a}あ").unwrap(),
        vec![ReadingPosition(0), ReadingPosition(2), ReadingPosition(3)]
    );
    assert_eq!(display_end("今日").unwrap(), DisplayUtf16Position(2));
    assert_eq!(
        reading_slice("𠮷あ", ReadingPosition(1), ReadingPosition(2)),
        Some("あ".into())
    );
    assert_eq!(
        reading_slice("あ", ReadingPosition(u32::MAX), ReadingPosition(u32::MAX)),
        None
    );
}

#[test]
fn shared_snapshot_and_receipt_fixtures_reject_loss_and_wrong_learning_material() {
    let data = fixture();
    for case in data["examples"].as_array().unwrap() {
        let wire = &case["wire"];
        let result = match wire["result"].as_str() {
            Some("SnapshotResult" | "SnapshotEnhancement") => {
                let reading = wire["reading"].as_str().unwrap();
                let clauses =
                    serde_json::from_value::<Vec<WireClause>>(wire["clauses"].clone()).unwrap();
                let encoded = serde_json::to_vec(&clauses).unwrap();
                assert_eq!(
                    serde_json::from_slice::<Vec<WireClause>>(&encoded).unwrap(),
                    clauses
                );
                validate_clauses(
                    reading,
                    &clauses,
                    ReadingPosition(0),
                    ReadingPosition(reading.chars().count() as u32),
                    wire["text"].as_str().unwrap(),
                )
            }
            _ if wire["method"] == "CommitReceipt" => {
                let receipt: CommitReceipt =
                    serde_json::from_value(wire["params"].clone()).unwrap();
                let encoded = serde_json::to_vec(&receipt).unwrap();
                assert_eq!(
                    serde_json::from_slice::<CommitReceipt>(&encoded).unwrap(),
                    receipt
                );
                receipt.validate(|token, interval| {
                    data["token_catalog"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|entry| {
                            entry["token"] == token
                                && entry["surface"] == interval.surface
                                && entry["reading_start"] == interval.reading_start.0
                                && entry["reading_end"] == interval.reading_end.0
                        })
                })
            }
            _ => continue,
        };
        assert_eq!(
            result.is_ok(),
            case["expect"] == "accept",
            "{}: {result:?}",
            case["id"]
        );
    }
}

#[derive(Deserialize)]
struct RawFixtures {
    examples: Vec<RawCase>,
}
#[derive(Deserialize)]
struct RawCase {
    id: String,
    expect: String,
    wire: Box<serde_json::value::RawValue>,
}

#[test]
fn production_protocol_decodes_every_shared_fixture_without_numeric_rounding() {
    use ipc::protocol::{Request, Response, PROTO_VERSION};
    assert_eq!(PROTO_VERSION, 9);
    let data: RawFixtures = serde_json::from_str(include_str!(
        "../../../docs/design/clause-navigation-p2/wire-fixtures.json"
    ))
    .unwrap();
    for case in data.examples {
        let wire = case.wire.get();
        let head: Value = serde_json::from_str(wire).unwrap();
        if head.get("method").is_some() {
            let decoded = serde_json::from_str::<Request>(wire);
            if case.expect == "reject-decode" {
                assert!(decoded.is_err(), "{}", case.id);
                continue;
            }
            let request = decoded.unwrap_or_else(|error| panic!("{}: {error}", case.id));
            assert_eq!(
                serde_json::from_str::<Request>(&serde_json::to_string(&request).unwrap()).unwrap(),
                request
            );
        } else {
            let response = serde_json::from_str::<Response>(wire)
                .unwrap_or_else(|error| panic!("{}: {error}", case.id));
            assert_eq!(
                serde_json::from_str::<Response>(&serde_json::to_string(&response).unwrap())
                    .unwrap(),
                response
            );
            if let Response::SnapshotResult {
                clause_data, text, ..
            }
            | Response::SnapshotEnhancement {
                clause_data, text, ..
            } = response
            {
                assert_eq!(
                    clause_data.validate(&text).is_ok(),
                    case.expect == "accept",
                    "{}",
                    case.id
                );
            }
        }
    }
}

#[test]
fn shared_request_keys_reject_bad_numbers_without_rounding_u64() {
    // Parse raw payloads: converting overflow JSON through a floating Value would hide wire errors.
    let data: RawFixtures = serde_json::from_str(include_str!(
        "../../../docs/design/clause-navigation-p2/wire-fixtures.json"
    ))
    .unwrap();
    #[derive(Deserialize)]
    struct Envelope {
        method: Option<String>,
        params: Option<Box<serde_json::value::RawValue>>,
    }
    for case in data.examples {
        let envelope: Envelope = serde_json::from_str(case.wire.get()).unwrap();
        if envelope.method.as_deref() != Some("ClauseCandidates") {
            continue;
        }
        let request =
            serde_json::from_str::<ClauseCandidatesRequest>(envelope.params.unwrap().get());
        assert_eq!(
            request.is_ok(),
            case.expect != "reject-decode",
            "{}",
            case.id
        );
        if let Ok(request) = request {
            request.validate().unwrap();
            assert_eq!(
                serde_json::from_str::<ClauseCandidatesRequest>(
                    &serde_json::to_string(&request).unwrap()
                )
                .unwrap(),
                request
            );
            if case.id == "u64-max" {
                assert_eq!(request.key.request_id, u64::MAX);
            }
        }
    }
}

#[test]
fn boundary_reply_must_match_exact_ids_ranges_and_legal_scalar_boundaries() {
    let data = fixture();
    let cases = data["examples"].as_array().unwrap();
    let request: ConvertClausesRequest = serde_json::from_value(
        cases
            .iter()
            .find(|c| c["id"] == "boundary-request")
            .unwrap()["wire"]["params"]
            .clone(),
    )
    .unwrap();
    let mut clauses: Vec<WireClause> = serde_json::from_value(
        cases.iter().find(|c| c["id"] == "boundary-Ready").unwrap()["wire"]["clauses"].clone(),
    )
    .unwrap();
    request.validate_result(&clauses).unwrap();
    clauses[0].id = ClauseId(1);
    assert!(request.validate_result(&clauses).is_err());
    clauses[0].id = ClauseId(4);
    clauses[0].reading_end = ReadingPosition(3);
    assert!(request.validate_result(&clauses).is_err());
    let split = [WireClause {
        id: ClauseId(1),
        reading_start: ReadingPosition(0),
        reading_end: ReadingPosition(1),
        state: ClauseState::Reading,
        surface: "か".into(),
        candidate_token: None,
    }];
    assert_eq!(
        validate_clauses(
            "か\u{3099}",
            &split,
            ReadingPosition(0),
            ReadingPosition(2),
            "か"
        ),
        Err(ClauseValidationError::Range)
    );
}

#[test]
fn shared_result_variants_keep_required_status_payloads() {
    let data = fixture();
    for case in data["examples"].as_array().unwrap() {
        let wire = &case["wire"];
        match wire["result"].as_str() {
            Some("ClauseCandidatesResult") => {
                let status: ClauseCandidatesStatus = serde_json::from_value(wire.clone()).unwrap();
                assert_eq!(
                    serde_json::from_str::<ClauseCandidatesStatus>(
                        &serde_json::to_string(&status).unwrap()
                    )
                    .unwrap(),
                    status
                );
            }
            Some("ConvertClausesResult") => {
                let status: ConvertClausesStatus = serde_json::from_value(wire.clone()).unwrap();
                assert_eq!(
                    serde_json::from_str::<ConvertClausesStatus>(
                        &serde_json::to_string(&status).unwrap()
                    )
                    .unwrap(),
                    status
                );
            }
            Some("CommitReceiptAck") => {
                let status: ReceiptStatus = serde_json::from_value(wire.clone()).unwrap();
                assert_eq!(
                    serde_json::from_str::<ReceiptStatus>(&serde_json::to_string(&status).unwrap())
                        .unwrap(),
                    status
                );
            }
            _ => {}
        }
    }
    assert!(serde_json::from_str::<ClauseCandidatesStatus>(r#"{"status":"Ready"}"#).is_err());
    assert!(serde_json::from_str::<ReceiptStatus>(r#"{"status":"Rejected"}"#).is_err());
    let mut wire = data["examples"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == "snapshot-mixed")
        .unwrap()["wire"]["clauses"][1]
        .clone();
    wire.as_object_mut().unwrap().remove("candidate_token");
    assert!(serde_json::from_value::<WireClause>(wire).is_err());
    let mut receipt = data["examples"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == "commit-mixed")
        .unwrap()["wire"]["params"]
        .clone();
    receipt.as_object_mut().unwrap().remove("sentence_token");
    assert!(serde_json::from_value::<CommitReceipt>(receipt).is_err());
}

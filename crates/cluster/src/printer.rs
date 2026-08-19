//! A CRD's own printer columns.
//!
//! `kubectl get certificates` prints READY, SECRET and AGE because
//! cert-manager's CustomResourceDefinition says so, not because `kubectl` knows
//! what cert-manager is. Every CRD version may declare
//! `additionalPrinterColumns`: a heading, an OpenAPI type, a JSONPath into the
//! object, and a priority. This module turns those declarations into the same
//! [`ColumnSpec`]s every other kind produces, and evaluates them against each
//! object to fill the cells in.
//!
//! What it deliberately matches, because the target user reads `kubectl` output
//! all day and a column that disagrees is worse than no column:
//!
//! * a value the JSONPath does not find renders `<none>`, and a value of the
//!   wrong type for its declared type renders `<none>` too;
//! * a `date` renders as an age — `5m`, `2d` — not as a timestamp;
//! * a column with a priority above zero is hidden, as `kubectl` hides one
//!   until asked for `-o wide`.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use jsonpath_rust::parser::model::JpQuery;
use jsonpath_rust::parser::parse_json_path;
use jsonpath_rust::query::js_path_process;
use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::{
    CustomResourceColumnDefinition, CustomResourceDefinition,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
use periscope_bridge::{ColumnSpec, KindId};
use serde_json::Value;

use crate::columns::to_system_time;

/// What `kubectl` prints where a printer column found no value.
const MISSING: &str = "<none>";

/// What `kubectl` prints where a `date` column's value is not a timestamp.
const INVALID: &str = "<invalid>";

/// How a printer column's value is rendered, from its declared OpenAPI type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Format {
    /// `string`, and the fallback for a type nobody recognises.
    Text,
    /// `integer` or `number`.
    Numeric,
    /// `boolean`.
    Boolean,
    /// `date`: an RFC 3339 timestamp, printed as the time since it.
    Date,
}

impl Format {
    /// Reads the `type` a CRD declared for a column.
    ///
    /// An unrecognised type is text rather than an error: the CRD is the
    /// cluster's, not ours, and refusing to render a column because its author
    /// wrote something the OpenAPI spec does not list would lose data that is
    /// perfectly printable.
    fn parse(declared: &str) -> Self {
        match declared {
            "integer" | "number" => Self::Numeric,
            "boolean" => Self::Boolean,
            "date" => Self::Date,
            _ => Self::Text,
        }
    }

    /// A width for a column nobody declared one for.
    ///
    /// CRDs say what a column is called and where its value lives, never how
    /// wide it should be, so the declared type is the only thing to go on.
    fn width(self) -> u32 {
        match self {
            Self::Boolean | Self::Date => 90,
            Self::Numeric => 100,
            Self::Text => 160,
        }
    }
}

/// One printer column, with its JSONPath already parsed.
///
/// Parsing happens once, when discovery reads the CRD, rather than once per
/// cell: a 10,000-row listing evaluates every column on every object, and
/// re-parsing the expression each time would put a `pest` parse in that loop.
#[derive(Clone, Debug)]
pub struct PrinterColumn {
    /// Heading, upper-cased the way `kubectl` upper-cases a CRD's `Sync Status`.
    name: Arc<str>,
    /// How to render the value.
    format: Format,
    /// Where the value is.
    path: JpQuery,
}

impl PrinterColumn {
    /// The column as the table renders it.
    fn spec(&self) -> ColumnSpec {
        let spec = ColumnSpec::fixed(&self.name, self.format.width());
        // A declared `integer` or `number` reads down its right edge, like
        // every count column the built-in kinds define.
        match self.format {
            Format::Numeric => spec.numeric(),
            _ => spec,
        }
    }

    /// This column's cell for one object, as of `now`.
    fn cell(&self, data: &Value, now: SystemTime) -> Arc<str> {
        // A filter such as `[?(@.type=="Ready")]` can match more than one
        // element; the apiserver takes the first and so does this.
        let found = js_path_process(&self.path, data)
            .ok()
            .and_then(|found| found.into_iter().next())
            .map(|found| found.val);

        let Some(value) = found else {
            return Arc::from(MISSING);
        };

        match self.format {
            Format::Text => match value.as_str() {
                Some(text) => Arc::from(text),
                None => Arc::from(MISSING),
            },
            Format::Numeric => match value {
                Value::Number(number) => Arc::from(number.to_string()),
                _ => Arc::from(MISSING),
            },
            Format::Boolean => match value.as_bool() {
                Some(flag) => Arc::from(if flag { "true" } else { "false" }),
                None => Arc::from(MISSING),
            },
            Format::Date => Arc::from(elapsed_since(value, now)),
        }
    }
}

/// Renders a `date` column: the time since its timestamp, as `kubectl` does.
///
/// A timestamp in the future is `<invalid>` rather than a negative age, which
/// is what `kubectl`'s `HumanDuration` does with one — and the second of slack
/// is its rule too, so a clock a hair ahead of the apiserver does not turn a
/// freshly created object's age into an error.
fn elapsed_since(value: &Value, now: SystemTime) -> String {
    // Parsed by `k8s-openapi`'s own `Time`, so a printer column reads a
    // timestamp exactly as the rest of the app reads `creationTimestamp`.
    let Ok(timestamp) = serde_json::from_value::<Time>(value.clone()) else {
        return INVALID.to_owned();
    };

    let created = to_system_time(&timestamp);
    match now.duration_since(created) {
        Ok(elapsed) => periscope_bridge::format::age(elapsed),
        Err(ahead) if ahead.duration() <= Duration::from_secs(1) => {
            periscope_bridge::format::age(Duration::ZERO)
        }
        Err(_) => INVALID.to_owned(),
    }
}

/// The columns a CRD declares for one of its versions.
///
/// Empty when the CRD serves no such version or declares no columns for it —
/// both perfectly ordinary, and both meaning "fall back to the generic table".
pub fn declared(
    crd: &CustomResourceDefinition,
    version: &str,
) -> Vec<CustomResourceColumnDefinition> {
    crd.spec
        .versions
        .iter()
        .find(|served| served.name == version)
        .and_then(|served| served.additional_printer_columns.clone())
        .unwrap_or_default()
}

/// Parses a CRD's declarations into columns that can be rendered.
///
/// Anything that cannot be rendered is dropped with a warning naming the kind
/// and the column, never silently: a CRD is the cluster's, its JSONPath may be
/// a dialect this does not implement, and the rest of the table must still
/// list. An empty result means "use the generic projection".
pub fn compile(kind: &KindId, declared: &[CustomResourceColumnDefinition]) -> Arc<[PrinterColumn]> {
    let columns: Vec<PrinterColumn> = declared
        .iter()
        .filter(|column| is_visible(kind, column))
        .filter_map(|column| match parse(column) {
            Ok(parsed) => Some(parsed),
            Err(reason) => {
                tracing::warn!(
                    %kind,
                    column = column.name,
                    json_path = column.json_path,
                    %reason,
                    "ignoring a printer column whose JSONPath could not be parsed"
                );
                None
            }
        })
        .collect();

    Arc::from(columns)
}

/// Whether a declared column earns a place in the table.
///
/// Two reasons it might not:
///
/// * `kubectl` hides every column with a priority above zero unless asked for
///   `-o wide`. There is no wide listing yet; when there is, this is the test
///   it relaxes, and `compile` is what it re-runs to widen a table.
/// * a CRD that declares its own AGE — cert-manager's do, and so do most —
///   would otherwise render beside the AGE column the table appends to every
///   kind, twice over from the same field.
fn is_visible(kind: &KindId, column: &CustomResourceColumnDefinition) -> bool {
    let priority = column.priority.unwrap_or(0);
    if priority > 0 {
        tracing::debug!(
            %kind,
            column = column.name,
            priority,
            "hiding a low-priority printer column"
        );
        return false;
    }

    !(column.name.eq_ignore_ascii_case("age")
        && column.type_ == "date"
        && column.json_path == ".metadata.creationTimestamp")
}

/// Parses one declaration, or says what about it could not be parsed.
fn parse(column: &CustomResourceColumnDefinition) -> Result<PrinterColumn, String> {
    Ok(PrinterColumn {
        name: Arc::from(column.name.to_uppercase()),
        format: Format::parse(&column.type_),
        path: parse_json_path(&rooted(&column.json_path)).map_err(|error| error.to_string())?,
    })
}

/// Rewrites a CRD's JSONPath into the form the parser accepts.
///
/// `additionalPrinterColumns` declares "a simple JSON path" relative to the
/// object — `.spec.secretName` — while RFC 9535, which `jsonpath-rust`
/// implements, wants the root named: `$.spec.secretName`. That is the whole
/// difference for the expressions CRDs actually write, filters included.
fn rooted(json_path: &str) -> String {
    match json_path.strip_prefix('$') {
        Some(_) => json_path.to_owned(),
        None => format!("${json_path}"),
    }
}

/// The [`ColumnSpec`]s for a compiled set of printer columns.
pub fn specs(columns: &[PrinterColumn]) -> Arc<[ColumnSpec]> {
    columns.iter().map(PrinterColumn::spec).collect()
}

/// The cells for one object, in column order.
pub fn cells(columns: &[PrinterColumn], data: &Value, now: SystemTime) -> Vec<Arc<str>> {
    columns
        .iter()
        .map(|column| column.cell(data, now))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn certificates() -> KindId {
        KindId::new("cert-manager.io", "v1", "Certificate", "certificates")
    }

    /// cert-manager's own CRD, trimmed to the parts this module reads.
    fn certificate_crd() -> CustomResourceDefinition {
        serde_json::from_value(json!({
            "metadata": { "name": "certificates.cert-manager.io" },
            "spec": {
                "group": "cert-manager.io",
                "names": { "kind": "Certificate", "plural": "certificates" },
                "scope": "Namespaced",
                "versions": [{
                    "name": "v1",
                    "served": true,
                    "storage": true,
                    "additionalPrinterColumns": [
                        { "name": "Ready", "type": "string",
                          "jsonPath": ".status.conditions[?(@.type==\"Ready\")].status" },
                        { "name": "Secret", "type": "string", "jsonPath": ".spec.secretName" },
                        { "name": "Issuer", "type": "string", "jsonPath": ".spec.issuerRef.name",
                          "priority": 1 },
                        { "name": "Status", "type": "string", "priority": 1,
                          "jsonPath": ".status.conditions[?(@.type==\"Ready\")].message" },
                        { "name": "Age", "type": "date", "jsonPath": ".metadata.creationTimestamp" }
                    ]
                }]
            }
        }))
        .expect("fixture is a valid CustomResourceDefinition")
    }

    fn compiled() -> Arc<[PrinterColumn]> {
        compile(&certificates(), &declared(&certificate_crd(), "v1"))
    }

    fn column(name: &str, type_: &str, json_path: &str) -> CustomResourceColumnDefinition {
        CustomResourceColumnDefinition {
            name: name.to_owned(),
            type_: type_.to_owned(),
            json_path: json_path.to_owned(),
            ..CustomResourceColumnDefinition::default()
        }
    }

    fn headings(columns: &[PrinterColumn]) -> Vec<String> {
        specs(columns)
            .iter()
            .map(|spec| spec.name.to_string())
            .collect()
    }

    fn rendered(columns: &[PrinterColumn], data: &Value) -> Vec<String> {
        cells(columns, data, SystemTime::UNIX_EPOCH)
            .iter()
            .map(|cell| cell.to_string())
            .collect()
    }

    #[test]
    fn a_crds_declared_columns_become_the_kinds_headings() {
        assert_eq!(headings(&compiled()), ["READY", "SECRET"]);
    }

    #[test]
    fn a_version_the_crd_does_not_serve_declares_nothing() {
        assert!(declared(&certificate_crd(), "v1alpha2").is_empty());
    }

    #[test]
    fn low_priority_columns_are_hidden_the_way_kubectl_hides_them() {
        // ISSUER and STATUS are priority 1: `kubectl get certificates` shows
        // neither, and `-o wide` — which does not exist here yet — is what
        // would.
        let headings = headings(&compiled());
        assert!(!headings.contains(&"ISSUER".to_owned()), "{headings:?}");
        assert!(!headings.contains(&"STATUS".to_owned()), "{headings:?}");
    }

    #[test]
    fn a_crds_own_age_column_does_not_duplicate_the_tables_age_column() {
        // Every table already ends in AGE, computed from the same field.
        assert!(!headings(&compiled()).contains(&"AGE".to_owned()));
    }

    #[test]
    fn a_filter_expression_picks_the_condition_it_names() {
        let cells = rendered(
            &compiled(),
            &json!({
                "metadata": { "name": "web-tls", "namespace": "default" },
                "spec": { "secretName": "web-tls" },
                "status": { "conditions": [
                    { "type": "Issuing", "status": "False" },
                    { "type": "Ready", "status": "True" }
                ]}
            }),
        );

        assert_eq!(cells, ["True", "web-tls"]);
    }

    #[test]
    fn a_value_that_is_not_there_reads_as_none_rather_than_blank() {
        // A blank cell and a cell whose value is the empty string look the same
        // and mean different things; `kubectl` distinguishes them and so does
        // this.
        let cells = rendered(
            &compiled(),
            &json!({ "metadata": { "name": "web-tls" }, "spec": {} }),
        );

        assert_eq!(cells, ["<none>", "<none>"]);
    }

    #[test]
    fn every_type_a_crd_may_declare_renders_as_kubectl_renders_it() {
        let columns = compile(
            &KindId::new("example.com", "v1", "Widget", "widgets"),
            &[
                column("Size", "integer", ".spec.size"),
                column("Ratio", "number", ".spec.ratio"),
                column("Enabled", "boolean", ".spec.enabled"),
                column("Phase", "string", ".status.phase"),
            ],
        );

        let cells = rendered(
            &columns,
            &json!({
                "spec": { "size": 3, "ratio": 1.5, "enabled": false },
                "status": { "phase": "Running" }
            }),
        );
        assert_eq!(cells, ["3", "1.5", "false", "Running"]);
    }

    #[test]
    fn a_value_of_the_wrong_declared_type_reads_as_missing() {
        // A CRD whose declaration and whose objects disagree: `kubectl` prints
        // nothing for the cell rather than the JSON that happens to be there.
        let columns = compile(
            &KindId::new("example.com", "v1", "Widget", "widgets"),
            &[
                column("Size", "integer", ".spec.size"),
                column("Owner", "string", ".spec.owner"),
            ],
        );

        let cells = rendered(
            &columns,
            &json!({ "spec": { "size": "three", "owner": { "name": "team" } } }),
        );
        assert_eq!(cells, ["<none>", "<none>"]);
    }

    #[test]
    fn a_date_column_renders_as_an_age() {
        let columns = compile(
            &KindId::new("acme.cert-manager.io", "v1", "Order", "orders"),
            &[column("Expires", "date", ".status.notAfter")],
        );

        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(90 * 60);
        let cells = cells(
            &columns,
            &json!({ "status": { "notAfter": "1970-01-01T00:00:00Z" } }),
            now,
        );
        assert_eq!(&*cells[0], "90m");
    }

    #[test]
    fn a_date_in_the_future_is_invalid_rather_than_a_negative_age() {
        let columns = compile(
            &KindId::new("acme.cert-manager.io", "v1", "Order", "orders"),
            &[column("Expires", "date", ".status.notAfter")],
        );

        let cells = cells(
            &columns,
            &json!({ "status": { "notAfter": "2999-01-01T00:00:00Z" } }),
            SystemTime::UNIX_EPOCH + Duration::from_secs(1),
        );
        assert_eq!(&*cells[0], "<invalid>");
    }

    #[test]
    fn a_date_column_that_is_not_a_timestamp_says_so() {
        let columns = compile(
            &KindId::new("example.com", "v1", "Widget", "widgets"),
            &[column("When", "date", ".spec.when")],
        );

        let cells = rendered(&columns, &json!({ "spec": { "when": "tomorrow" } }));
        assert_eq!(cells, ["<invalid>"]);
    }

    #[test]
    fn an_indexed_path_reads_the_element_it_names() {
        let columns = compile(
            &KindId::new("example.com", "v1", "Widget", "widgets"),
            &[column("First", "string", ".status.conditions[0].status")],
        );

        let cells = rendered(
            &columns,
            &json!({ "status": { "conditions": [{ "status": "True" }, { "status": "False" }] } }),
        );
        assert_eq!(cells, ["True"]);
    }

    #[test]
    fn a_json_path_that_cannot_be_parsed_costs_its_column_and_nothing_else() {
        // A CRD is the cluster's, and `kubectl`'s JSONPath dialect is not
        // exactly RFC 9535; one expression this cannot read must not cost the
        // listing.
        let columns = compile(
            &KindId::new("example.com", "v1", "Widget", "widgets"),
            &[
                column("Broken", "string", ".spec.[[["),
                column("Size", "integer", ".spec.size"),
            ],
        );

        assert_eq!(headings(&columns), ["SIZE"]);
        assert_eq!(rendered(&columns, &json!({ "spec": { "size": 7 } })), ["7"]);
    }

    #[test]
    fn a_path_already_naming_the_root_is_left_alone() {
        assert_eq!(rooted(".spec.size"), "$.spec.size");
        assert_eq!(rooted("$.spec.size"), "$.spec.size");
    }

    #[test]
    fn headings_are_upper_cased_the_way_every_other_column_is() {
        let columns = compile(
            &KindId::new("argoproj.io", "v1alpha1", "Application", "applications"),
            &[column("Sync Status", "string", ".status.sync.status")],
        );

        assert_eq!(headings(&columns), ["SYNC STATUS"]);
    }

    #[test]
    fn a_crd_that_declares_no_columns_compiles_to_none() {
        assert!(compile(&certificates(), &[]).is_empty());
    }
}

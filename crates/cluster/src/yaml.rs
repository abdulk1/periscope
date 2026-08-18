//! Reading YAML back in.
//!
//! Periscope writes YAML itself (`detail.rs`) but does not parse it; an edited
//! object has to be parsed, and that is a job for a real parser. `serde-saphyr`
//! is what `kube` already uses to read kubeconfig, so this adds no new
//! dependency to the tree.

use serde_json::Value;

/// Parses a YAML document into JSON, which is what the apiserver takes.
///
/// The error is the parser's own message, because a person fixing their YAML
/// needs the line and column, not "invalid input".
pub fn parse(yaml: &str) -> Result<Value, String> {
    if yaml.trim().is_empty() {
        return Err("the document is empty".to_owned());
    }

    serde_saphyr::from_str::<Value>(yaml).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_kubernetes_object_round_trips() {
        let parsed = parse(
            "apiVersion: apps/v1\n\
             kind: Deployment\n\
             metadata:\n  \
               name: api\n  \
               namespace: payments\n\
             spec:\n  \
               replicas: 3\n",
        )
        .expect("parses");

        assert_eq!(parsed["kind"], json!("Deployment"));
        assert_eq!(parsed["metadata"]["name"], json!("api"));
        assert_eq!(parsed["spec"]["replicas"], json!(3));
    }

    #[test]
    fn lists_and_nested_maps_survive() {
        let parsed = parse(
            "spec:\n  \
               containers:\n  \
               - name: api\n    \
                 ports:\n    \
                 - containerPort: 8080\n",
        )
        .expect("parses");

        assert_eq!(parsed["spec"]["containers"][0]["name"], json!("api"));
        assert_eq!(
            parsed["spec"]["containers"][0]["ports"][0]["containerPort"],
            json!(8080)
        );
    }

    #[test]
    fn quoted_strings_stay_strings() {
        // The YAML writer quotes these for exactly this reason; the parser has
        // to hold up its end.
        let parsed = parse("version: \"1.20\"\nenabled: \"true\"\n").expect("parses");

        assert_eq!(parsed["version"], json!("1.20"));
        assert_eq!(parsed["enabled"], json!("true"));
    }

    #[test]
    fn a_block_scalar_keeps_its_newlines() {
        let parsed = parse("ca.crt: |-\n  -----BEGIN\n  MIIC\n  -----END\n").expect("parses");
        assert_eq!(parsed["ca.crt"], json!("-----BEGIN\nMIIC\n-----END"));
    }

    #[test]
    fn broken_yaml_reports_where_it_broke() {
        let error = parse("metadata:\n  name: api\n   bad-indent: yes\n")
            .expect_err("malformed YAML is an error");
        // The parser's own message, so the person fixing it gets a position.
        assert!(!error.is_empty());
    }

    #[test]
    fn an_empty_document_is_refused_rather_than_applied_as_nothing() {
        // Applying an empty document would be a request to blank the object.
        assert!(parse("   \n").is_err());
    }
}

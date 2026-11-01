use wasm_dap_core::evaluator::{Expr, parse};

#[test]
fn parse_integer() {
    assert_eq!(parse("42").unwrap(), Expr::Integer(42));
}

#[test]
fn parse_negative_integer() {
    assert_eq!(parse("-7").unwrap(), Expr::Integer(-7));
}

#[test]
fn parse_float() {
    match parse("2.5").unwrap() {
        Expr::Float(f) => assert!((f - 2.5).abs() < 1e-9),
        _ => panic!("expected float"),
    }
}

#[test]
fn parse_string_double_quotes() {
    assert_eq!(
        parse("\"hello\"").unwrap(),
        Expr::String("hello".to_string())
    );
}

#[test]
fn parse_string_single_quotes() {
    assert_eq!(parse("'world'").unwrap(), Expr::String("world".to_string()));
}

#[test]
fn parse_identifier() {
    assert_eq!(parse("foo").unwrap(), Expr::Identifier("foo".to_string()));
}

#[test]
fn parse_call_with_args() {
    match parse("add(2, 3)").unwrap() {
        Expr::Call { name, args } => {
            assert_eq!(name, "add");
            assert_eq!(args, vec![Expr::Integer(2), Expr::Integer(3)]);
        }
        _ => panic!("expected call"),
    }
}

#[test]
fn parse_call_no_args() {
    match parse("boop()").unwrap() {
        Expr::Call { name, args } => {
            assert_eq!(name, "boop");
            assert!(args.is_empty());
        }
        _ => panic!("expected call"),
    }
}

#[test]
fn parse_rejects_trailing_input() {
    assert!(parse("42 extra").is_err());
}

#[test]
fn parse_rejects_empty() {
    assert!(parse("").is_err());
}

#[test]
fn parse_rejects_unterminated_string() {
    assert!(parse("\"unterminated").is_err());
}

#[test]
fn parse_bounds_nesting_and_length() {
    let too_deep = |expr: String| {
        parse(&expr)
            .unwrap_err()
            .to_string()
            .contains("nested too deeply")
    };
    let nested = format!("{}1{}", "(".repeat(90), ")".repeat(90));
    assert_eq!(parse(&nested).unwrap(), Expr::Integer(1));
    assert!(too_deep(format!("{}1{}", "(".repeat(150), ")".repeat(150))));
    assert!(too_deep(format!("{}x", "!".repeat(150))));

    assert!(parse(&["1"; 100].join("+")).is_ok());
    assert!(too_deep(["1"; 150].join("+")));
    assert!(too_deep(format!("x{}", ".f".repeat(150))));
    assert!(too_deep(format!("x{}", "[0]".repeat(150))));

    let too_long = parse(&"1".repeat(1025)).unwrap_err().to_string();
    assert!(too_long.contains("longer than"), "{too_long}");
}

use super::{
    coerce_cell_number, eval_ast, eval_binary_float, eval_binary_float_with_complex_fallback,
    eval_cell_inner, eval_sum, number_from_bool, parse_number_literal, Ast, EvalResult, Number,
};
use crate::formula::number::{mod_trunc_toward_zero, round_rational_decimal_places};
use crate::grid::{CellAddr, GridBox as Grid, MainRange};
use chrono::{Datelike, Local, NaiveDate, NaiveDateTime, Timelike};
use num_traits::Zero;
use std::hash::{Hash, Hasher};

#[optimize(speed)]
pub(crate) fn eval_builtin(
    name: &str,
    args: &[Ast],
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
) -> EvalResult {
    let u = name.to_ascii_uppercase();
    match u.as_str() {
        "SUM" => {
            if args.len() != 1 {
                return EvalResult::Error("ARGS");
            }
            eval_sum(&args[0], grid, visiting, budget, allow_templates)
        }
        "AVERAGE" => eval_numeric_aggregate(
            &args,
            grid,
            visiting,
            bindings,
            budget,
            allow_templates,
            NumericAgg::Average,
        ),
        "MIN" => eval_numeric_aggregate(
            &args,
            grid,
            visiting,
            bindings,
            budget,
            allow_templates,
            NumericAgg::Min,
        ),
        "MAX" => eval_numeric_aggregate(
            &args,
            grid,
            visiting,
            bindings,
            budget,
            allow_templates,
            NumericAgg::Max,
        ),
        "COUNT" => eval_count(&args, grid, visiting, bindings, budget, allow_templates),
        "COUNTA" => eval_counta(&args, grid, visiting, bindings, budget, allow_templates),
        "COUNTBLANK" => eval_countblank(&args, grid, visiting, bindings, budget, allow_templates),
        "PRODUCT" => eval_numeric_aggregate(
            &args,
            grid,
            visiting,
            bindings,
            budget,
            allow_templates,
            NumericAgg::Product,
        ),
        "ABS" => eval_abs(&args, grid, visiting, bindings, budget, allow_templates),
        "ROUND" => eval_round(&args, grid, visiting, bindings, budget, allow_templates),
        "MOD" => eval_mod(&args, grid, visiting, bindings, budget, allow_templates),
        "POWER" => {
            if args.len() != 2 {
                EvalResult::Error("ARGS")
            } else {
                eval_binary_float_with_complex_fallback(
                    &args[0],
                    &args[1],
                    grid,
                    visiting,
                    bindings,
                    budget,
                    allow_templates,
                    f64::powf,
                    |x, y| x.powc(y),
                )
            }
        }
        "SQRT" => eval_unary_numeric_with_complex_fallback(
            &args,
            grid,
            visiting,
            bindings,
            budget,
            allow_templates,
            f64::sqrt,
            num_complex::Complex64::sqrt,
        ),
        "PI" => {
            if !args.is_empty() {
                EvalResult::Error("ARGS")
            } else {
                EvalResult::Number(Number::from_f64_unchecked(std::f64::consts::PI))
            }
        }
        "TRUE" => {
            if !args.is_empty() {
                EvalResult::Error("ARGS")
            } else {
                EvalResult::Bool(true)
            }
        }
        "FALSE" => {
            if !args.is_empty() {
                EvalResult::Error("ARGS")
            } else {
                EvalResult::Bool(false)
            }
        }
        "SIN" => eval_unary_numeric(
            &args,
            grid,
            visiting,
            bindings,
            budget,
            allow_templates,
            f64::sin,
        ),
        "COS" => eval_unary_numeric(
            &args,
            grid,
            visiting,
            bindings,
            budget,
            allow_templates,
            f64::cos,
        ),
        "TRIM" => eval_trim(&args, grid, visiting, bindings, budget, allow_templates),
        "UPPER" => eval_upper(&args, grid, visiting, bindings, budget, allow_templates),
        "LOWER" => eval_lower(&args, grid, visiting, bindings, budget, allow_templates),
        "PROPER" => eval_proper(&args, grid, visiting, bindings, budget, allow_templates),
        "SUBSTITUTE" => eval_substitute(&args, grid, visiting, bindings, budget, allow_templates),
        "REPLACE" => eval_replace(&args, grid, visiting, bindings, budget, allow_templates),
        "FIND" => eval_find(&args, grid, visiting, bindings, budget, allow_templates),
        "SEARCH" => eval_search(&args, grid, visiting, bindings, budget, allow_templates),
        "TEXT" => eval_text(&args, grid, visiting, bindings, budget, allow_templates),
        "ISNUMBER" => eval_isnumber(&args, grid, visiting, bindings, budget, allow_templates),
        "ISTEXT" => eval_istext(&args, grid, visiting, bindings, budget, allow_templates),
        "ISBLANK" => eval_isblank(&args, grid, visiting, bindings, budget, allow_templates),
        "TYPE" => eval_type(&args, grid, visiting, bindings, budget, allow_templates),
        "TYPEOF" => eval_typeof(&args, grid, visiting, bindings, budget, allow_templates),
        "ISERROR" => eval_iserror(&args, grid, visiting, bindings, budget, allow_templates),
        "ISNA" => eval_isna(&args, grid, visiting, bindings, budget, allow_templates),
        "TODAY" => eval_today(&args),
        "NOW" => eval_now(&args),
        "DATE" => eval_date(&args, grid, visiting, bindings, budget, allow_templates),
        "YEAR" => eval_year(&args, grid, visiting, bindings, budget, allow_templates),
        "MONTH" => eval_month(&args, grid, visiting, bindings, budget, allow_templates),
        "DAY" => eval_day(&args, grid, visiting, bindings, budget, allow_templates),
        "HOUR" => eval_hour(&args, grid, visiting, bindings, budget, allow_templates),
        "MINUTE" => eval_minute(&args, grid, visiting, bindings, budget, allow_templates),
        "SECOND" => eval_second(&args, grid, visiting, bindings, budget, allow_templates),
        "ROUNDUP" => eval_roundup(&args, grid, visiting, bindings, budget, allow_templates),
        "ROUNDDOWN" => eval_rounddown(&args, grid, visiting, bindings, budget, allow_templates),
        "INT" => eval_int(&args, grid, visiting, bindings, budget, allow_templates),
        "CEILING" => eval_ceiling(&args, grid, visiting, bindings, budget, allow_templates),
        "FLOOR" => eval_floor(&args, grid, visiting, bindings, budget, allow_templates),
        "RAND" => eval_rand(&args, grid, visiting, bindings, budget, allow_templates),
        "RANDBETWEEN" => eval_randbetween(&args, grid, visiting, bindings, budget, allow_templates),
        "SUMPRODUCT" => eval_sumproduct(&args, grid, visiting, bindings, budget, allow_templates),
        "LEN" => eval_len(&args, grid, visiting, bindings, budget, allow_templates),
        "LEFT" => eval_left(&args, grid, visiting, bindings, budget, allow_templates),
        "RIGHT" => eval_right(&args, grid, visiting, bindings, budget, allow_templates),
        "MID" => eval_mid(&args, grid, visiting, bindings, budget, allow_templates),
        "CONCAT" => eval_concat(&args, grid, visiting, bindings, budget, allow_templates),
        "TEXTJOIN" => eval_textjoin(&args, grid, visiting, bindings, budget, allow_templates),
        "AND" => eval_and(&args, grid, visiting, bindings, budget, allow_templates),
        "OR" => eval_or(&args, grid, visiting, bindings, budget, allow_templates),
        "NOT" => eval_not(&args, grid, visiting, bindings, budget, allow_templates),
        "IFERROR" => eval_iferror(&args, grid, visiting, bindings, budget, allow_templates),
        "IFNA" => eval_ifna(&args, grid, visiting, bindings, budget, allow_templates),
        "COUNTIF" => eval_countif(&args, grid, visiting, bindings, budget, allow_templates),
        "SUMIF" => eval_sumif(&args, grid, visiting, bindings, budget, allow_templates),
        "COUNTIFS" => eval_countifs(&args, grid, visiting, bindings, budget, allow_templates),
        "SUMIFS" => eval_sumifs(&args, grid, visiting, bindings, budget, allow_templates),
        "AVERAGEIFS" => eval_averageifs(&args, grid, visiting, bindings, budget, allow_templates),
        "LOOKUP" => eval_lookup(&args, grid, visiting, bindings, budget, allow_templates),
        "VLOOKUP" => eval_vlookup(&args, grid, visiting, bindings, budget, allow_templates),
        "XLOOKUP" => eval_xlookup(&args, grid, visiting, bindings, budget, allow_templates),
        "MATCH" => eval_match(&args, grid, visiting, bindings, budget, allow_templates),
        "XMATCH" => eval_xmatch(&args, grid, visiting, bindings, budget, allow_templates),
        "INDEX" => eval_index(&args, grid, visiting, bindings, budget, allow_templates),
        "LET" => eval_let(&args, grid, visiting, bindings, budget, allow_templates),
        "CHOOSE" => eval_choose(&args, grid, visiting, bindings, budget, allow_templates),
        "SWITCH" => eval_switch(&args, grid, visiting, bindings, budget, allow_templates),
        "IFS" => eval_ifs(&args, grid, visiting, bindings, budget, allow_templates),
        "SEQUENCE" => eval_sequence(&args, grid, visiting, bindings, budget, allow_templates),
        "FILTER" => eval_filter(&args, grid, visiting, bindings, budget, allow_templates),
        "UNIQUE" => eval_unique(&args, grid, visiting, bindings, budget, allow_templates),
        "SORT" => eval_sort(&args, grid, visiting, bindings, budget, allow_templates),
        "SORTBY" => eval_sortby(&args, grid, visiting, bindings, budget, allow_templates),
        "TAKE" => eval_take(&args, grid, visiting, bindings, budget, allow_templates),
        "DROP" => eval_drop(&args, grid, visiting, bindings, budget, allow_templates),
        "CHOOSECOLS" => eval_choosecols(&args, grid, visiting, bindings, budget, allow_templates),
        "CHOOSEROWS" => eval_chooserows(&args, grid, visiting, bindings, budget, allow_templates),
        "IF" => {
            if args.len() != 3 {
                return EvalResult::Error("ARGS");
            }
            let cond = eval_ast(&args[0], grid, visiting, bindings, budget, allow_templates);
            let pick = super::truthy(cond);
            if pick {
                eval_ast(&args[1], grid, visiting, bindings, budget, allow_templates)
            } else {
                eval_ast(&args[2], grid, visiting, bindings, budget, allow_templates)
            }
        }
        _ => EvalResult::Error("FUNC"),
    }
}

enum NumericAgg {
    Average,
    Min,
    Max,
    Product,
}

fn eval_abs(
    args: &[Ast],
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
) -> EvalResult {
    if args.len() != 1 {
        return EvalResult::Error("ARGS");
    }
    let ea = eval_ast(
        &args[0],
        grid,
        visiting,
        bindings,
        budget,
        allow_templates,
    )
    .scalar_coerce();
    match coerce_cell_number(ea) {
        Ok(n) => EvalResult::Number(n.abs()),
        Err(e) => e,
    }
}

fn round_builtin_decimal_places(nd: &Number) -> Option<i32> {
    let f = nd.to_f64();
    if !f.is_finite() {
        return None;
    }
    let p = f.round().clamp(-30.0, 30.0);
    Some(p as i32)
}

fn eval_round(
    args: &[Ast],
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
) -> EvalResult {
    if args.len() != 2 {
        return EvalResult::Error("ARGS");
    }
    let ea = eval_ast(
        &args[0],
        grid,
        visiting,
        bindings,
        budget,
        allow_templates,
    )
    .scalar_coerce();
    let na = match coerce_cell_number(ea) {
        Ok(n) => n,
        Err(e) => return e,
    };
    let eb = eval_ast(
        &args[1],
        grid,
        visiting,
        bindings,
        budget,
        allow_templates,
    )
    .scalar_coerce();
    let digits_n = match coerce_cell_number(eb) {
        Ok(n) => n,
        Err(e) => return e,
    };
    if let Number::Exact(r) = &na {
        if let Some(place) = round_builtin_decimal_places(&digits_n) {
            return EvalResult::Number(Number::Exact(round_rational_decimal_places(r, place)));
        }
    }
    let n = na.to_f64();
    let digits = digits_n.to_f64();
    let factor = 10f64.powf(digits);
    EvalResult::Number(Number::from_f64_unchecked((n * factor).round() / factor))
}

fn eval_mod(
    args: &[Ast],
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
) -> EvalResult {
    if args.len() != 2 {
        return EvalResult::Error("ARGS");
    }
    let ea = eval_ast(
        &args[0],
        grid,
        visiting,
        bindings,
        budget,
        allow_templates,
    )
    .scalar_coerce();
    let na = match coerce_cell_number(ea) {
        Ok(n) => n,
        Err(e) => return e,
    };
    let eb = eval_ast(
        &args[1],
        grid,
        visiting,
        bindings,
        budget,
        allow_templates,
    )
    .scalar_coerce();
    let nb = match coerce_cell_number(eb) {
        Ok(n) => n,
        Err(e) => return e,
    };
    match (&na, &nb) {
        (Number::Exact(a), Number::Exact(b)) => {
            if b.is_zero() {
                return EvalResult::Number(Number::from_f64_unchecked(f64::NAN));
            }
            if let Some(rem) = mod_trunc_toward_zero(a, b) {
                return EvalResult::Number(Number::Exact(rem));
            }
            EvalResult::Number(Number::from_f64_unchecked(f64::NAN))
        }
        _ => eval_binary_float(
            &args[0],
            &args[1],
            grid,
            visiting,
            bindings,
            budget,
            allow_templates,
            std::ops::Rem::rem,
        ),
    }
}

#[optimize(speed)]
fn eval_numeric_aggregate(
    args: &[Ast],
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
    kind: NumericAgg,
) -> EvalResult {
    if args.is_empty() {
        return EvalResult::Error("ARGS");
    }
    let mut nums: Vec<Number> = Vec::new();
    for a in args {
        match collect_numeric_values(a, grid, visiting, bindings, budget, allow_templates) {
            Ok(n) => nums.extend(n),
            Err(e) => return EvalResult::Error(e),
        }
    }
    match kind {
        NumericAgg::Average => {
            if nums.is_empty() {
                EvalResult::Error("DIV0")
            } else {
                let sum = nums
                    .iter()
                    .cloned()
                    .fold(Number::exact_zero(), |a, b| a.add(b));
                EvalResult::Number(sum.div(Number::from_i64(nums.len() as i64)))
            }
        }
        NumericAgg::Min => nums
            .into_iter()
            .min_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
            .map(EvalResult::Number)
            .unwrap_or(EvalResult::Number(Number::exact_zero())),
        NumericAgg::Max => nums
            .into_iter()
            .max_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
            .map(EvalResult::Number)
            .unwrap_or(EvalResult::Number(Number::exact_zero())),
        NumericAgg::Product => EvalResult::Number(
            nums.into_iter()
                .fold(Number::one(), |acc, n| acc.mul(n)),
        ),
    }
}

fn eval_unary_numeric(
    args: &[Ast],
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
    f: fn(f64) -> f64,
) -> EvalResult {
    if args.len() != 1 {
        return EvalResult::Error("ARGS");
    }
    match numeric_value(eval_ast(
        &args[0],
        grid,
        visiting,
        bindings,
        budget,
        allow_templates,
    )) {
        Some(n) => EvalResult::Number(Number::from_f64_unchecked(f(n))),
        None => EvalResult::Error("VALUE"),
    }
}

fn eval_unary_numeric_with_complex_fallback(
    args: &[Ast],
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
    real: fn(f64) -> f64,
    complex: fn(num_complex::Complex64) -> num_complex::Complex64,
) -> EvalResult {
    if args.len() != 1 {
        return EvalResult::Error("ARGS");
    }
    let evaluated = eval_ast(
        &args[0],
        grid,
        visiting,
        bindings,
        budget,
        allow_templates,
    )
    .scalar_coerce();
    let number = match coerce_cell_number(evaluated) {
        Ok(n) => n,
        Err(e) => return e,
    };
    EvalResult::Number(number.apply_unary_f64_with_complex_fallback(real, complex))
}

pub(crate) fn parse_date_serial_literal(s: &str) -> Option<f64> {
    let t = s.trim();
    #[cfg(test)]
    {
        // Narrow test-only logging to likely date-like inputs to avoid noisy output.
        if t.contains("2001/01/01") || t.contains("2001-01-01") || t.contains('/') {
            crate::debug_log::log(&format!("DEBUG parse_date_serial_literal called: raw={:?} trimmed={:?}", s, t));
        }
    }
    if t.len() != 10 {
        #[cfg(test)]
        {
            if t.contains("2001/01/01") || t.contains("2001-01-01") || t.contains('/') {
                crate::debug_log::log(&format!("DEBUG parse_date_serial_literal: length mismatch: {}", t.len()));
            }
        }
        return None;
    }
    let bytes = t.as_bytes();
    // Accept common separators '-', '/', and '\\' between components so inputs
    // like "2001/01/01" (found in fixtures) are recognized as date literals.
    let is_sep = |b: u8| b == b'-' || b == b'/' || b == b'\\';
    if !is_sep(bytes[4]) || !is_sep(bytes[7]) {
        #[cfg(test)]
        {
            if t.contains("2001/01/01") || t.contains("2001-01-01") || t.contains('/') {
                crate::debug_log::log(&format!("DEBUG parse_date_serial_literal: separator mismatch: {:?}", t));
            }
        }
        return None;
    }
    let year = t[0..4].parse::<i32>().ok()?;
    let month = t[5..7].parse::<u32>().ok()?;
    let day = t[8..10].parse::<u32>().ok()?;
    let res = NaiveDate::from_ymd_opt(year, month, day).map(date_to_serial);
    #[cfg(test)]
    {
        if t.contains("2001/01/01") || t.contains("2001-01-01") || t.contains('/') {
            crate::debug_log::log(&format!("DEBUG parse_date_serial_literal: parsed {}-{}-{} => {:?}", year, month, day, res));
        }
    }
    res
}

pub(crate) fn parse_numeric_or_date_literal(s: &str) -> Option<Number> {
    #[cfg(test)]
    {
        // Log only likely candidates to keep test output focused.
        if s.contains("2001/01/01") || s.contains("2001-01-01") || s.contains('/') {
            crate::debug_log::log(&format!("DEBUG parse_numeric_or_date_literal called: {:?}", s));
        }
    }
    let n = parse_number_literal(s).or_else(|| parse_date_serial_literal(s).map(Number::approx));
    #[cfg(test)]
    {
        if s.contains("2001/01/01") || s.contains("2001-01-01") || s.contains('/') {
            crate::debug_log::log(&format!("DEBUG parse_numeric_or_date_literal result for {:?}: {:?}", s, n.as_ref().map(|x| x.to_f64())));
        }
    }
    n
}

fn eval_trim(
    args: &[Ast],
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
) -> EvalResult {
    if args.len() != 1 {
        return EvalResult::Error("ARGS");
    }
    match eval_ast(&args[0], grid, visiting, bindings, budget, allow_templates).scalar_coerce() {
        EvalResult::Number(n) => EvalResult::Text(trim_spaces(&format!("{n}"))),
        EvalResult::Bool(true) => EvalResult::Text("TRUE".into()),
        EvalResult::Bool(false) => EvalResult::Text("FALSE".into()),
        EvalResult::Text(s) => EvalResult::Text(trim_spaces(&s)),
        EvalResult::Error(e) => EvalResult::Error(e),
        EvalResult::Array(_) => EvalResult::Error("CALC"),
    }
}

fn eval_upper(
    args: &[Ast],
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
) -> EvalResult {
    if args.len() != 1 {
        return EvalResult::Error("ARGS");
    }
    match eval_text_arg(&args[0], grid, visiting, bindings, budget, allow_templates) {
        Ok(s) => EvalResult::Text(s.to_ascii_uppercase()),
        Err(e) => EvalResult::Error(e),
    }
}

fn eval_lower(
    args: &[Ast],
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
) -> EvalResult {
    if args.len() != 1 {
        return EvalResult::Error("ARGS");
    }
    match eval_text_arg(&args[0], grid, visiting, bindings, budget, allow_templates) {
        Ok(s) => EvalResult::Text(s.to_ascii_lowercase()),
        Err(e) => EvalResult::Error(e),
    }
}

fn eval_proper(
    args: &[Ast],
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
) -> EvalResult {
    if args.len() != 1 {
        return EvalResult::Error("ARGS");
    }
    let s = match eval_text_arg(&args[0], grid, visiting, bindings, budget, allow_templates) {
        Ok(s) => s,
        Err(e) => return EvalResult::Error(e),
    };
    let mut out = String::with_capacity(s.len());
    let mut new_word = true;
    for ch in s.chars() {
        if ch.is_ascii_alphabetic() {
            if new_word {
                out.push(ch.to_ascii_uppercase());
            } else {
                out.push(ch.to_ascii_lowercase());
            }
            new_word = false;
        } else {
            out.push(ch);
            new_word = ch.is_whitespace() || ch == '_' || ch == '-';
        }
    }
    EvalResult::Text(out)
}

fn eval_substitute(
    args: &[Ast],
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
) -> EvalResult {
    if args.len() < 3 || args.len() > 4 {
        return EvalResult::Error("ARGS");
    }
    let text = match eval_text_arg(&args[0], grid, visiting, bindings, budget, allow_templates) {
        Ok(s) => s,
        Err(e) => return EvalResult::Error(e),
    };
    let old = match eval_text_arg(&args[1], grid, visiting, bindings, budget, allow_templates) {
        Ok(s) => s,
        Err(e) => return EvalResult::Error(e),
    };
    let new = match eval_text_arg(&args[2], grid, visiting, bindings, budget, allow_templates) {
        Ok(s) => s,
        Err(e) => return EvalResult::Error(e),
    };
    let instance = if args.len() == 4 {
        match numeric_value(eval_ast(
            &args[3],
            grid,
            visiting,
            bindings,
            budget,
            allow_templates,
        )) {
            Some(n) if n >= 1.0 => n as usize,
            _ => return EvalResult::Error("VALUE"),
        }
    } else {
        0
    };
    if instance == 0 {
        EvalResult::Text(text.replace(&old, &new))
    } else {
        let mut out = String::new();
        let mut count = 0usize;
        let mut i = 0usize;
        while let Some(pos) = text[i..].find(&old) {
            let start = i + pos;
            let end = start + old.len();
            out.push_str(&text[i..start]);
            count += 1;
            if count == instance {
                out.push_str(&new);
                out.push_str(&text[end..]);
                return EvalResult::Text(out);
            }
            out.push_str(&old);
            i = end;
        }
        EvalResult::Text(text)
    }
}

fn eval_replace(
    args: &[Ast],
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
) -> EvalResult {
    if args.len() != 4 {
        return EvalResult::Error("ARGS");
    }
    let text = match eval_text_arg(&args[0], grid, visiting, bindings, budget, allow_templates) {
        Ok(s) => s,
        Err(e) => return EvalResult::Error(e),
    };
    let start = match numeric_value(eval_ast(
        &args[1],
        grid,
        visiting,
        bindings,
        budget,
        allow_templates,
    )) {
        Some(n) if n >= 1.0 => n as usize - 1,
        _ => return EvalResult::Error("VALUE"),
    };
    let len = match numeric_value(eval_ast(
        &args[2],
        grid,
        visiting,
        bindings,
        budget,
        allow_templates,
    )) {
        Some(n) if n >= 0.0 => n as usize,
        _ => return EvalResult::Error("VALUE"),
    };
    let new = match eval_text_arg(&args[3], grid, visiting, bindings, budget, allow_templates) {
        Ok(s) => s,
        Err(e) => return EvalResult::Error(e),
    };
    let chars: Vec<char> = text.chars().collect();
    let start = start.min(chars.len());
    let end = (start + len).min(chars.len());
    let mut out = String::new();
    out.extend(chars[..start].iter());
    out.push_str(&new);
    out.extend(chars[end..].iter());
    EvalResult::Text(out)
}

fn eval_find(
    args: &[Ast],
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
) -> EvalResult {
    if args.len() < 2 || args.len() > 3 {
        return EvalResult::Error("ARGS");
    }
    let needle = match eval_text_arg(&args[0], grid, visiting, bindings, budget, allow_templates) {
        Ok(s) => s,
        Err(e) => return EvalResult::Error(e),
    };
    let hay = match eval_text_arg(&args[1], grid, visiting, bindings, budget, allow_templates) {
        Ok(s) => s,
        Err(e) => return EvalResult::Error(e),
    };
    let start = if args.len() == 3 {
        match numeric_value(eval_ast(
            &args[2],
            grid,
            visiting,
            bindings,
            budget,
            allow_templates,
        )) {
            Some(n) if n >= 1.0 => n as usize - 1,
            _ => return EvalResult::Error("VALUE"),
        }
    } else {
        0
    };
    let start = start.min(hay.len());
    match hay[start..].find(&needle) {
        Some(pos) => EvalResult::Number(Number::from_i64((start + pos + 1) as i64)),
        None => EvalResult::Error("VALUE"),
    }
}

fn eval_search(
    args: &[Ast],
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
) -> EvalResult {
    if args.len() < 2 || args.len() > 3 {
        return EvalResult::Error("ARGS");
    }
    let needle = match eval_text_arg(&args[0], grid, visiting, bindings, budget, allow_templates) {
        Ok(s) => s.to_ascii_lowercase(),
        Err(e) => return EvalResult::Error(e),
    };
    let hay = match eval_text_arg(&args[1], grid, visiting, bindings, budget, allow_templates) {
        Ok(s) => s.to_ascii_lowercase(),
        Err(e) => return EvalResult::Error(e),
    };
    let start = if args.len() == 3 {
        match numeric_value(eval_ast(
            &args[2],
            grid,
            visiting,
            bindings,
            budget,
            allow_templates,
        )) {
            Some(n) if n >= 1.0 => n as usize - 1,
            _ => return EvalResult::Error("VALUE"),
        }
    } else {
        0
    };
    let start = start.min(hay.len());
    match hay[start..].find(&needle) {
        Some(pos) => EvalResult::Number(Number::from_i64((start + pos + 1) as i64)),
        None => EvalResult::Error("VALUE"),
    }
}

fn eval_text(
    args: &[Ast],
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
) -> EvalResult {
    if args.len() != 2 {
        return EvalResult::Error("ARGS");
    }
    let value = match eval_ast(&args[0], grid, visiting, bindings, budget, allow_templates)
        .scalar_coerce()
    {
        EvalResult::Number(n) => n,
        EvalResult::Bool(b) => number_from_bool(b),
        EvalResult::Text(s) => match parse_numeric_or_date_literal(&s) {
            Some(n) => n,
            None => return EvalResult::Text(s),
        },
        EvalResult::Error(e) => return EvalResult::Error(e),
        EvalResult::Array(_) => return EvalResult::Error("CALC"),
    };
    let fmt = match eval_text_arg(&args[1], grid, visiting, bindings, budget, allow_templates) {
        Ok(s) => s,
        Err(e) => return EvalResult::Error(e),
    };
    let vf = value.to_f64();
    if let Some(decimals) = fmt.strip_prefix("0.") {
        let digits = decimals.len();
        EvalResult::Text(format!("{:.*}", digits, vf))
    } else if fmt == "0" {
        EvalResult::Text(format!("{}", vf.round() as i64))
    } else {
        EvalResult::Text(value.to_string())
    }
}

fn eval_today(args: &[Ast]) -> EvalResult {
    if !args.is_empty() {
        return EvalResult::Error("ARGS");
    }
    let now = Local::now().date_naive();
    EvalResult::Number(Number::approx(date_to_serial(now)))
}

fn eval_now(args: &[Ast]) -> EvalResult {
    if !args.is_empty() {
        return EvalResult::Error("ARGS");
    }
    let now = Local::now().naive_local();
    EvalResult::Number(Number::approx(datetime_to_serial(now)))
}

fn eval_date(
    args: &[Ast],
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
) -> EvalResult {
    if args.len() != 3 {
        return EvalResult::Error("ARGS");
    }
    let year = match numeric_value(eval_ast(
        &args[0],
        grid,
        visiting,
        bindings,
        budget,
        allow_templates,
    )) {
        Some(n) => n as i32,
        None => return EvalResult::Error("VALUE"),
    };
    let month = match numeric_value(eval_ast(
        &args[1],
        grid,
        visiting,
        bindings,
        budget,
        allow_templates,
    )) {
        Some(n) => n as u32,
        None => return EvalResult::Error("VALUE"),
    };
    let day = match numeric_value(eval_ast(
        &args[2],
        grid,
        visiting,
        bindings,
        budget,
        allow_templates,
    )) {
        Some(n) => n as u32,
        None => return EvalResult::Error("VALUE"),
    };
    match NaiveDate::from_ymd_opt(year, month, day) {
        Some(d) => EvalResult::Number(Number::approx(date_to_serial(d))),
        None => EvalResult::Error("VALUE"),
    }
}

fn eval_year(
    args: &[Ast],
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
) -> EvalResult {
    date_component(
        args,
        grid,
        visiting,
        bindings,
        budget,
        allow_templates,
        |dt| dt.year() as f64,
    )
}

fn eval_month(
    args: &[Ast],
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
) -> EvalResult {
    date_component(
        args,
        grid,
        visiting,
        bindings,
        budget,
        allow_templates,
        |dt| dt.month() as f64,
    )
}

fn eval_day(
    args: &[Ast],
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
) -> EvalResult {
    date_component(
        args,
        grid,
        visiting,
        bindings,
        budget,
        allow_templates,
        |dt| dt.day() as f64,
    )
}

fn eval_hour(
    args: &[Ast],
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
) -> EvalResult {
    time_component(
        args,
        grid,
        visiting,
        bindings,
        budget,
        allow_templates,
        |dt| dt.hour() as f64,
    )
}

fn eval_minute(
    args: &[Ast],
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
) -> EvalResult {
    time_component(
        args,
        grid,
        visiting,
        bindings,
        budget,
        allow_templates,
        |dt| dt.minute() as f64,
    )
}

fn eval_second(
    args: &[Ast],
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
) -> EvalResult {
    time_component(
        args,
        grid,
        visiting,
        bindings,
        budget,
        allow_templates,
        |dt| dt.second() as f64,
    )
}

fn eval_roundup(
    args: &[Ast],
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
) -> EvalResult {
    round_with_mode(
        args,
        grid,
        visiting,
        bindings,
        budget,
        allow_templates,
        true,
    )
}

fn eval_rounddown(
    args: &[Ast],
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
) -> EvalResult {
    round_with_mode(
        args,
        grid,
        visiting,
        bindings,
        budget,
        allow_templates,
        false,
    )
}

fn eval_int(
    args: &[Ast],
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
) -> EvalResult {
    if args.len() != 1 {
        return EvalResult::Error("ARGS");
    }
    match numeric_value(eval_ast(
        &args[0],
        grid,
        visiting,
        bindings,
        budget,
        allow_templates,
    )) {
        Some(n) => EvalResult::Number(Number::from_f64_unchecked(n.floor())),
        None => EvalResult::Error("VALUE"),
    }
}

fn eval_ceiling(
    args: &[Ast],
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
) -> EvalResult {
    if args.len() != 2 {
        return EvalResult::Error("ARGS");
    }
    let n = match numeric_value(eval_ast(
        &args[0],
        grid,
        visiting,
        bindings,
        budget,
        allow_templates,
    )) {
        Some(v) => v,
        None => return EvalResult::Error("VALUE"),
    };
    let significance = match numeric_value(eval_ast(
        &args[1],
        grid,
        visiting,
        bindings,
        budget,
        allow_templates,
    )) {
        Some(v) if v != 0.0 => v,
        _ => return EvalResult::Error("VALUE"),
    };
    EvalResult::Number(Number::from_f64_unchecked(
        (n / significance).ceil() * significance,
    ))
}

fn eval_floor(
    args: &[Ast],
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
) -> EvalResult {
    if args.len() != 2 {
        return EvalResult::Error("ARGS");
    }
    let n = match numeric_value(eval_ast(
        &args[0],
        grid,
        visiting,
        bindings,
        budget,
        allow_templates,
    )) {
        Some(v) => v,
        None => return EvalResult::Error("VALUE"),
    };
    let significance = match numeric_value(eval_ast(
        &args[1],
        grid,
        visiting,
        bindings,
        budget,
        allow_templates,
    )) {
        Some(v) if v != 0.0 => v,
        _ => return EvalResult::Error("VALUE"),
    };
    EvalResult::Number(Number::from_f64_unchecked(
        (n / significance).floor() * significance,
    ))
}

fn eval_rand(
    args: &[Ast],
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    _bindings: &mut Vec<(String, EvalResult)>,
    _budget: &mut usize,
    _allow_templates: bool,
) -> EvalResult {
    if !args.is_empty() {
        return EvalResult::Error("ARGS");
    }
    EvalResult::Number(Number::from_f64_unchecked(deterministic_rand(
        grid,
        current_addr_hash(visiting),
        None,
        None,
    )))
}

fn eval_randbetween(
    args: &[Ast],
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
) -> EvalResult {
    if args.len() != 2 {
        return EvalResult::Error("ARGS");
    }
    let low = match numeric_value(eval_ast(
        &args[0],
        grid,
        visiting,
        bindings,
        budget,
        allow_templates,
    )) {
        Some(v) => v.floor() as i64,
        None => return EvalResult::Error("VALUE"),
    };
    let high = match numeric_value(eval_ast(
        &args[1],
        grid,
        visiting,
        bindings,
        budget,
        allow_templates,
    )) {
        Some(v) => v.floor() as i64,
        None => return EvalResult::Error("VALUE"),
    };
    if low > high {
        return EvalResult::Error("VALUE");
    }
    let n = deterministic_rand(
        grid,
        current_addr_hash(visiting),
        Some(low as u64),
        Some(high as u64),
    );
    let span = (high - low + 1) as f64;
    EvalResult::Number(Number::from_f64_unchecked(low as f64 + (n * span).floor()))
}

fn date_component<F>(
    args: &[Ast],
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
    f: F,
) -> EvalResult
where
    F: Fn(&NaiveDateTime) -> f64,
{
    if args.len() != 1 {
        return EvalResult::Error("ARGS");
    }
    let serial = match numeric_value(eval_ast(
        &args[0],
        grid,
        visiting,
        bindings,
        budget,
        allow_templates,
    )) {
        Some(v) => v,
        None => return EvalResult::Error("VALUE"),
    };
    match serial_to_datetime(serial) {
        Some(dt) => EvalResult::Number(Number::from_f64_unchecked(f(&dt))),
        None => EvalResult::Error("VALUE"),
    }
}

fn time_component<F>(
    args: &[Ast],
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
    f: F,
) -> EvalResult
where
    F: Fn(&NaiveDateTime) -> f64,
{
    date_component(args, grid, visiting, bindings, budget, allow_templates, f)
}

fn round_with_mode(
    args: &[Ast],
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
    up: bool,
) -> EvalResult {
    if args.len() != 1 && args.len() != 2 {
        return EvalResult::Error("ARGS");
    }
    let n = match numeric_value(eval_ast(
        &args[0],
        grid,
        visiting,
        bindings,
        budget,
        allow_templates,
    )) {
        Some(v) => v,
        None => return EvalResult::Error("VALUE"),
    };
    let digits = if args.len() == 2 {
        match numeric_value(eval_ast(
            &args[1],
            grid,
            visiting,
            bindings,
            budget,
            allow_templates,
        )) {
            Some(v) => v,
            None => return EvalResult::Error("VALUE"),
        }
    } else {
        0.0
    };
    let factor = 10f64.powf(digits);
    let scaled = n * factor;
    let rounded = if up { scaled.ceil() } else { scaled.floor() };
    EvalResult::Number(Number::from_f64_unchecked(rounded / factor))
}

fn date_to_serial(date: NaiveDate) -> f64 {
    let epoch = NaiveDate::from_ymd_opt(1899, 12, 30).expect("valid epoch");
    (date - epoch).num_days() as f64
}

fn datetime_to_serial(dt: NaiveDateTime) -> f64 {
    let date_serial = date_to_serial(dt.date());
    date_serial + (dt.time().num_seconds_from_midnight() as f64) / 86_400.0
}

fn serial_to_datetime(serial: f64) -> Option<NaiveDateTime> {
    let epoch = NaiveDate::from_ymd_opt(1899, 12, 30)?.and_hms_opt(0, 0, 0)?;
    let days = serial.floor() as i64;
    let frac = serial - serial.floor();
    let secs = (frac * 86_400.0).round() as i64;
    epoch
        .checked_add_signed(chrono::Duration::days(days))?
        .checked_add_signed(chrono::Duration::seconds(secs))
}

fn current_addr_hash(visiting: &[CellAddr]) -> Option<u64> {
    visiting.last().map(|addr| {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        Hash::hash(addr, &mut hasher);
        hasher.finish()
    })
}

fn deterministic_rand(grid: &Grid, c: Option<u64>, a: Option<u64>, b: Option<u64>) -> f64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    Hash::hash(&grid.volatile_seed(), &mut hasher);
    Hash::hash(&c, &mut hasher);
    Hash::hash(&a, &mut hasher);
    Hash::hash(&b, &mut hasher);
    let v = hasher.finish();
    (v as f64) / (u64::MAX as f64)
}

fn eval_text_arg(
    ast: &Ast,
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
) -> Result<String, &'static str> {
    match eval_ast(ast, grid, visiting, bindings, budget, allow_templates).scalar_coerce() {
        EvalResult::Number(n) => Ok(n.to_string()),
        EvalResult::Bool(true) => Ok("TRUE".into()),
        EvalResult::Bool(false) => Ok("FALSE".into()),
        EvalResult::Text(s) => Ok(s),
        EvalResult::Error(e) => Err(e),
        EvalResult::Array(_) => Err("CALC"),
    }
}

#[optimize(speed)]
fn eval_sumproduct(
    args: &[Ast],
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
) -> EvalResult {
    if args.is_empty() {
        return EvalResult::Error("ARGS");
    }
    let mut matrices = Vec::new();
    for arg in args {
        match collect_matrix_values(arg, grid, visiting, bindings, budget, allow_templates) {
            Ok(m) => matrices.push(m),
            Err(e) => return EvalResult::Error(e),
        }
    }
    let rows = matrices[0].len();
    let cols = matrices[0].first().map(|r| r.len()).unwrap_or(0);
    if rows == 0 || cols == 0 {
        return EvalResult::Number(Number::exact_zero());
    }
    if matrices
        .iter()
        .any(|m| m.len() != rows || m.iter().any(|r| r.len() != cols))
    {
        return EvalResult::Error("ARGS");
    }
    let mut sum = Number::exact_zero();
    for r in 0..rows {
        for c in 0..cols {
            let mut prod = Number::one();
            for m in &matrices {
                let v = match m[r][c].clone().scalar_coerce() {
                    EvalResult::Number(n) => n,
                    EvalResult::Bool(b) => number_from_bool(b),
                    EvalResult::Text(s) => match parse_number_literal(&s) {
                        Some(n) => n,
                        None => return EvalResult::Error("VALUE"),
                    },
                    EvalResult::Error(e) => return EvalResult::Error(e),
                    EvalResult::Array(_) => return EvalResult::Error("CALC"),
                };
                prod = prod.mul(v);
            }
            sum = sum.add(prod);
        }
    }
    EvalResult::Number(sum)
}

fn eval_ifs(
    args: &[Ast],
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
) -> EvalResult {
    if args.len() < 2 || !args.len().is_multiple_of(2) {
        return EvalResult::Error("ARGS");
    }
    for pair in args.chunks(2) {
        let cond = eval_ast(&pair[0], grid, visiting, bindings, budget, allow_templates);
        if super::truthy(cond) {
            return eval_ast(&pair[1], grid, visiting, bindings, budget, allow_templates);
        }
    }
    EvalResult::Error("NA")
}

fn eval_iferror(
    args: &[Ast],
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
) -> EvalResult {
    if args.len() != 2 {
        return EvalResult::Error("ARGS");
    }
    let value = eval_ast(&args[0], grid, visiting, bindings, budget, allow_templates);
    if matches!(value, EvalResult::Error(_) | EvalResult::Array(_)) {
        eval_ast(&args[1], grid, visiting, bindings, budget, allow_templates)
    } else {
        value
    }
}

fn eval_ifna(
    args: &[Ast],
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
) -> EvalResult {
    if args.len() != 2 {
        return EvalResult::Error("ARGS");
    }
    let value = eval_ast(&args[0], grid, visiting, bindings, budget, allow_templates);
    match value {
        EvalResult::Error("NA") | EvalResult::Error("PARSE") => {
            eval_ast(&args[1], grid, visiting, bindings, budget, allow_templates)
        }
        _ => value,
    }
}

fn eval_len(
    args: &[Ast],
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
) -> EvalResult {
    if args.len() != 1 {
        return EvalResult::Error("ARGS");
    }
    match eval_ast(&args[0], grid, visiting, bindings, budget, allow_templates).scalar_coerce() {
        EvalResult::Text(s) => {
            EvalResult::Number(Number::from_i64(s.chars().count() as i64))
        }
        EvalResult::Bool(b) => EvalResult::Number(Number::from_i64(
            (if b { "TRUE" } else { "FALSE" }).chars().count() as i64,
        )),
        EvalResult::Number(n) => EvalResult::Number(Number::from_i64(
            format!("{n}").chars().count() as i64,
        )),
        EvalResult::Error(e) => EvalResult::Error(e),
        EvalResult::Array(_) => EvalResult::Error("CALC"),
    }
}

fn eval_left(
    args: &[Ast],
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
) -> EvalResult {
    if args.len() != 1 && args.len() != 2 {
        return EvalResult::Error("ARGS");
    }
    let text = match eval_ast(&args[0], grid, visiting, bindings, budget, allow_templates)
        .scalar_coerce()
    {
        EvalResult::Text(s) => s,
        EvalResult::Number(n) => n.to_string(),
        EvalResult::Bool(true) => "TRUE".into(),
        EvalResult::Bool(false) => "FALSE".into(),
        EvalResult::Error(e) => return EvalResult::Error(e),
        EvalResult::Array(_) => return EvalResult::Error("CALC"),
    };
    let n = if args.len() == 2 {
        match numeric_value(eval_ast(
            &args[1],
            grid,
            visiting,
            bindings,
            budget,
            allow_templates,
        )) {
            Some(v) if v >= 0.0 => v as usize,
            _ => return EvalResult::Error("VALUE"),
        }
    } else {
        1
    };
    EvalResult::Text(text.chars().take(n).collect())
}

fn eval_right(
    args: &[Ast],
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
) -> EvalResult {
    if args.len() != 1 && args.len() != 2 {
        return EvalResult::Error("ARGS");
    }
    let text = match eval_ast(&args[0], grid, visiting, bindings, budget, allow_templates)
        .scalar_coerce()
    {
        EvalResult::Text(s) => s,
        EvalResult::Number(n) => n.to_string(),
        EvalResult::Bool(true) => "TRUE".into(),
        EvalResult::Bool(false) => "FALSE".into(),
        EvalResult::Error(e) => return EvalResult::Error(e),
        EvalResult::Array(_) => return EvalResult::Error("CALC"),
    };
    let n = if args.len() == 2 {
        match numeric_value(eval_ast(
            &args[1],
            grid,
            visiting,
            bindings,
            budget,
            allow_templates,
        )) {
            Some(v) if v >= 0.0 => v as usize,
            _ => return EvalResult::Error("VALUE"),
        }
    } else {
        1
    };
    let chars: Vec<char> = text.chars().collect();
    let start = chars.len().saturating_sub(n);
    EvalResult::Text(chars[start..].iter().collect())
}

fn eval_mid(
    args: &[Ast],
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
) -> EvalResult {
    if args.len() != 3 {
        return EvalResult::Error("ARGS");
    }
    let text = match eval_ast(&args[0], grid, visiting, bindings, budget, allow_templates)
        .scalar_coerce()
    {
        EvalResult::Text(s) => s,
        EvalResult::Number(n) => n.to_string(),
        EvalResult::Bool(true) => "TRUE".into(),
        EvalResult::Bool(false) => "FALSE".into(),
        EvalResult::Error(e) => return EvalResult::Error(e),
        EvalResult::Array(_) => return EvalResult::Error("CALC"),
    };
    let start = match numeric_value(eval_ast(
        &args[1],
        grid,
        visiting,
        bindings,
        budget,
        allow_templates,
    )) {
        Some(v) if v >= 1.0 => v as usize,
        _ => return EvalResult::Error("VALUE"),
    };
    let len = match numeric_value(eval_ast(
        &args[2],
        grid,
        visiting,
        bindings,
        budget,
        allow_templates,
    )) {
        Some(v) if v >= 0.0 => v as usize,
        _ => return EvalResult::Error("VALUE"),
    };
    let chars: Vec<char> = text.chars().collect();
    let start = start.saturating_sub(1);
    EvalResult::Text(chars.into_iter().skip(start).take(len).collect())
}

fn eval_concat(
    args: &[Ast],
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
) -> EvalResult {
    let mut out = String::new();
    for arg in args {
        match eval_ast(arg, grid, visiting, bindings, budget, allow_templates).scalar_coerce() {
            EvalResult::Number(n) => out.push_str(&n.to_string()),
            EvalResult::Bool(true) => out.push_str("TRUE"),
            EvalResult::Bool(false) => out.push_str("FALSE"),
            EvalResult::Text(s) => out.push_str(&s),
            EvalResult::Error(e) => return EvalResult::Error(e),
            EvalResult::Array(_) => return EvalResult::Error("CALC"),
        }
    }
    EvalResult::Text(out)
}

fn eval_textjoin(
    args: &[Ast],
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
) -> EvalResult {
    if args.len() < 2 {
        return EvalResult::Error("ARGS");
    }
    let delim = match eval_ast(&args[0], grid, visiting, bindings, budget, allow_templates)
        .scalar_coerce()
    {
        EvalResult::Text(s) => s,
        EvalResult::Number(n) => n.to_string(),
        EvalResult::Bool(true) => "TRUE".into(),
        EvalResult::Bool(false) => "FALSE".into(),
        EvalResult::Error(e) => return EvalResult::Error(e),
        EvalResult::Array(_) => return EvalResult::Error("CALC"),
    };
    let mut parts = Vec::new();
    for arg in &args[1..] {
        let value =
            eval_ast(arg, grid, visiting, bindings, budget, allow_templates).scalar_coerce();
        match value {
            EvalResult::Number(n) => parts.push(n.to_string()),
            EvalResult::Bool(true) => parts.push("TRUE".into()),
            EvalResult::Bool(false) => parts.push("FALSE".into()),
            EvalResult::Text(s) => {
                if !s.is_empty() {
                    parts.push(s);
                }
            }
            EvalResult::Error(e) => return EvalResult::Error(e),
            EvalResult::Array(_) => return EvalResult::Error("CALC"),
        }
    }
    EvalResult::Text(parts.join(&delim))
}

fn eval_not(
    args: &[Ast],
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
) -> EvalResult {
    if args.len() != 1 {
        return EvalResult::Error("ARGS");
    }
    EvalResult::Number(if super::truthy(eval_ast(
        &args[0],
        grid,
        visiting,
        bindings,
        budget,
        allow_templates,
    )) {
        Number::exact_zero()
    } else {
        Number::one()
    })
}

fn eval_and(
    args: &[Ast],
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
) -> EvalResult {
    for arg in args {
        if !super::truthy(eval_ast(
            arg,
            grid,
            visiting,
            bindings,
            budget,
            allow_templates,
        )) {
            return EvalResult::Number(Number::exact_zero());
        }
    }
    EvalResult::Number(Number::one())
}

fn eval_or(
    args: &[Ast],
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
) -> EvalResult {
    for arg in args {
        if super::truthy(eval_ast(
            arg,
            grid,
            visiting,
            bindings,
            budget,
            allow_templates,
        )) {
            return EvalResult::Number(Number::one());
        }
    }
    EvalResult::Number(Number::exact_zero())
}

#[optimize(speed)]
fn eval_match(
    args: &[Ast],
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
) -> EvalResult {
    if args.len() < 2 || args.len() > 3 {
        return EvalResult::Error("ARGS");
    }
    if args.len() == 3 {
        let mt = match numeric_value(eval_ast(
            &args[2],
            grid,
            visiting,
            bindings,
            budget,
            allow_templates,
        )) {
            Some(n) => n,
            None => return EvalResult::Error("VALUE"),
        };
        if mt != 0.0 {
            return EvalResult::Error("NA");
        }
    }
    let lookup_value = match eval_ast(&args[0], grid, visiting, bindings, budget, allow_templates)
        .scalar_coerce()
    {
        EvalResult::Number(n) => LookupValue::Number(n),
        EvalResult::Bool(b) => LookupValue::Number(number_from_bool(b)),
        EvalResult::Text(s) => parse_number_literal(&s)
            .map(LookupValue::Number)
            .unwrap_or(LookupValue::Text(s)),
        EvalResult::Error(e) => return EvalResult::Error(e),
        EvalResult::Array(_) => return EvalResult::Error("CALC"),
    };
    let matrix =
        match collect_matrix_values(&args[1], grid, visiting, bindings, budget, allow_templates) {
            Ok(m) => m,
            Err(e) => return EvalResult::Error(e),
        };
    let mut idx = 1u32;
    for row in matrix {
        for cell in row {
            let candidate = match cell.scalar_coerce() {
                EvalResult::Number(n) => LookupValue::Number(n),
                EvalResult::Bool(b) => LookupValue::Number(number_from_bool(b)),
                EvalResult::Text(s) => parse_number_literal(&s)
                    .map(LookupValue::Number)
                    .unwrap_or(LookupValue::Text(s)),
                EvalResult::Error(_) => {
                    idx += 1;
                    continue;
                }
                EvalResult::Array(_) => {
                    idx += 1;
                    continue;
                }
            };
            if candidate == lookup_value {
                return EvalResult::Number(Number::from_i64(idx as i64));
            }
            idx += 1;
        }
    }
    EvalResult::Error("NA")
}

fn eval_xmatch(
    args: &[Ast],
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
) -> EvalResult {
    if args.len() < 2 || args.len() > 4 {
        return EvalResult::Error("ARGS");
    }
    let lookup_value = match eval_ast(&args[0], grid, visiting, bindings, budget, allow_templates)
        .scalar_coerce()
    {
        EvalResult::Number(n) => LookupValue::Number(n),
        EvalResult::Bool(b) => LookupValue::Number(number_from_bool(b)),
        EvalResult::Text(s) => parse_number_literal(&s)
            .map(LookupValue::Number)
            .unwrap_or(LookupValue::Text(s)),
        EvalResult::Error(e) => return EvalResult::Error(e),
        EvalResult::Array(_) => return EvalResult::Error("CALC"),
    };
    let matrix =
        match collect_matrix_values(&args[1], grid, visiting, bindings, budget, allow_templates) {
            Ok(m) => m,
            Err(e) => return EvalResult::Error(e),
        };
    let search_mode = if args.len() >= 3 {
        match numeric_value(eval_ast(
            &args[2],
            grid,
            visiting,
            bindings,
            budget,
            allow_templates,
        )) {
            Some(n) => n as i32,
            None => return EvalResult::Error("VALUE"),
        }
    } else {
        1
    };
    if args.len() >= 4 {
        let mode = match numeric_value(eval_ast(
            &args[3],
            grid,
            visiting,
            bindings,
            budget,
            allow_templates,
        )) {
            Some(n) => n as i32,
            None => return EvalResult::Error("VALUE"),
        };
        if mode != 0 {
            return EvalResult::Error("NA");
        }
    }
    let mut flat = Vec::new();
    for row in matrix {
        for cell in row {
            flat.push(cell.scalar_coerce());
        }
    }
    if search_mode >= 0 {
        for (i, cell) in flat.iter().enumerate() {
            let candidate = match cell.clone() {
                EvalResult::Number(n) => LookupValue::Number(n),
                EvalResult::Bool(b) => LookupValue::Number(number_from_bool(b)),
                EvalResult::Text(s) => parse_number_literal(&s)
                    .map(LookupValue::Number)
                    .unwrap_or(LookupValue::Text(s)),
                EvalResult::Error(_) => continue,
                EvalResult::Array(_) => continue,
            };
            if candidate == lookup_value {
                return EvalResult::Number(Number::from_i64((i + 1) as i64));
            }
        }
    } else {
        for (i, cell) in flat.iter().enumerate().rev() {
            let candidate = match cell.clone() {
                EvalResult::Number(n) => LookupValue::Number(n),
                EvalResult::Bool(b) => LookupValue::Number(number_from_bool(b)),
                EvalResult::Text(s) => parse_number_literal(&s)
                    .map(LookupValue::Number)
                    .unwrap_or(LookupValue::Text(s)),
                EvalResult::Error(_) => continue,
                EvalResult::Array(_) => continue,
            };
            if candidate == lookup_value {
                return EvalResult::Number(Number::from_i64((i + 1) as i64));
            }
        }
    }
    EvalResult::Error("NA")
}

#[optimize(speed)]
fn eval_index(
    args: &[Ast],
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
) -> EvalResult {
    if args.len() != 2 && args.len() != 3 {
        return EvalResult::Error("ARGS");
    }
    let matrix =
        match collect_matrix_values(&args[0], grid, visiting, bindings, budget, allow_templates) {
            Ok(m) => m,
            Err(e) => return EvalResult::Error(e),
        };
    if matrix.is_empty() || matrix[0].is_empty() {
        return EvalResult::Error("REF");
    }
    let row = match numeric_value(eval_ast(
        &args[1],
        grid,
        visiting,
        bindings,
        budget,
        allow_templates,
    )) {
        Some(n) if n >= 1.0 => n as usize,
        _ => return EvalResult::Error("VALUE"),
    };
    let col = if args.len() == 3 {
        match numeric_value(eval_ast(
            &args[2],
            grid,
            visiting,
            bindings,
            budget,
            allow_templates,
        )) {
            Some(n) if n >= 1.0 => n as usize,
            _ => return EvalResult::Error("VALUE"),
        }
    } else if matrix.len() == 1 {
        row
    } else {
        1
    };
    if row == 0 || col == 0 {
        return EvalResult::Error("REF");
    }
    if row > matrix.len() {
        return EvalResult::Error("REF");
    }
    let row_idx = row - 1;
    if col > matrix[row_idx].len() {
        return EvalResult::Error("REF");
    }
    matrix[row_idx][col - 1].clone()
}

fn eval_countifs(
    args: &[Ast],
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
) -> EvalResult {
    if args.len() < 2 || !args.len().is_multiple_of(2) {
        return EvalResult::Error("ARGS");
    }
    let pairs =
        match collect_criteria_pairs(args, grid, visiting, bindings, budget, allow_templates) {
            Ok(p) => p,
            Err(e) => return EvalResult::Error(e),
        };
    let Some((first_range, _)) = pairs.first() else {
        return EvalResult::Error("ARGS");
    };
    let mut count = 0usize;
    for dr in 0..range_height(first_range) {
        for dc in 0..range_width(first_range) {
            let mut ok = true;
            for (range, criteria) in &pairs {
                let addr = CellAddr::Main {
                    row: range.row_start + dr,
                    col: range.col_start + dc,
                };
                if !criteria_matches(criteria, grid, &addr, visiting, budget, allow_templates) {
                    ok = false;
                    break;
                }
            }
            if ok {
                count += 1;
            }
        }
    }
    EvalResult::Number(Number::from_i64(count as i64))
}

fn eval_sumifs(
    args: &[Ast],
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
) -> EvalResult {
    if args.len() < 3 || args.len().is_multiple_of(2) {
        return EvalResult::Error("ARGS");
    }
    let Some(sum_range) = as_main_range(&args[0]) else {
        return EvalResult::Error("RANGE");
    };
    let pairs = match collect_criteria_pairs(
        &args[1..],
        grid,
        visiting,
        bindings,
        budget,
        allow_templates,
    ) {
        Ok(p) => p,
        Err(e) => return EvalResult::Error(e),
    };
    if pairs.iter().any(|(range, _)| {
        range_height(range) != range_height(&sum_range)
            || range_width(range) != range_width(&sum_range)
    }) {
        return EvalResult::Error("ARGS");
    }
    let mut sum = Number::exact_zero();
    for dr in 0..range_height(&sum_range) {
        for dc in 0..range_width(&sum_range) {
            let mut ok = true;
            for (range, criteria) in &pairs {
                let addr = CellAddr::Main {
                    row: range.row_start + dr,
                    col: range.col_start + dc,
                };
                if !criteria_matches(criteria, grid, &addr, visiting, budget, allow_templates) {
                    ok = false;
                    break;
                }
            }
            if ok {
                let addr = CellAddr::Main {
                    row: sum_range.row_start + dr,
                    col: sum_range.col_start + dc,
                };
                if let Some(n) = super::summable_numeric(grid, &addr, visiting, budget) {
                    sum = sum.add(n);
                }
            }
        }
    }
    EvalResult::Number(sum)
}

fn eval_averageifs(
    args: &[Ast],
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
) -> EvalResult {
    match eval_sumifs(args, grid, visiting, bindings, budget, allow_templates) {
        EvalResult::Number(sum) => {
            let sum_range = match as_main_range(&args[0]) {
                Some(r) => r,
                None => return EvalResult::Error("RANGE"),
            };
            let pairs = match collect_criteria_pairs(
                &args[1..],
                grid,
                visiting,
                bindings,
                budget,
                allow_templates,
            ) {
                Ok(p) => p,
                Err(e) => return EvalResult::Error(e),
            };
            let mut count = 0usize;
            for dr in 0..range_height(&sum_range) {
                for dc in 0..range_width(&sum_range) {
                    let mut ok = true;
                    for (range, criteria) in &pairs {
                        let addr = CellAddr::Main {
                            row: range.row_start + dr,
                            col: range.col_start + dc,
                        };
                        if !criteria_matches(
                            criteria,
                            grid,
                            &addr,
                            visiting,
                            budget,
                            allow_templates,
                        ) {
                            ok = false;
                            break;
                        }
                    }
                    if ok {
                        let addr = CellAddr::Main {
                            row: sum_range.row_start + dr,
                            col: sum_range.col_start + dc,
                        };
                        if super::effective_numeric(grid, &addr, visiting, budget).is_some() {
                            count += 1;
                        }
                    }
                }
            }
            if count == 0 {
                EvalResult::Error("DIV0")
            } else {
                EvalResult::Number(sum.div(Number::from_i64(count as i64)))
            }
        }
        other => other,
    }
}

#[optimize(speed)]
fn eval_sort(
    args: &[Ast],
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
) -> EvalResult {
    if args.is_empty() || args.len() > 4 {
        return EvalResult::Error("ARGS");
    }
    let mut matrix =
        match collect_matrix_values(&args[0], grid, visiting, bindings, budget, allow_templates) {
            Ok(m) => m,
            Err(e) => return EvalResult::Error(e),
        };
    let sort_index = if args.len() >= 2 {
        match numeric_value(eval_ast(
            &args[1],
            grid,
            visiting,
            bindings,
            budget,
            allow_templates,
        )) {
            Some(n) if n >= 1.0 => n as usize,
            _ => return EvalResult::Error("VALUE"),
        }
    } else {
        1
    };
    let sort_order = if args.len() >= 3 {
        match numeric_value(eval_ast(
            &args[2],
            grid,
            visiting,
            bindings,
            budget,
            allow_templates,
        )) {
            Some(n) if n < 0.0 => -1,
            Some(_) => 1,
            None => return EvalResult::Error("VALUE"),
        }
    } else {
        1
    };
    let by_col = if args.len() == 4 {
        super::truthy(eval_ast(
            &args[3],
            grid,
            visiting,
            bindings,
            budget,
            allow_templates,
        ))
    } else {
        false
    };
    if by_col {
        transpose_matrix(&mut matrix);
    }
    let key_col = sort_index.saturating_sub(1);
    if matrix.is_empty() || key_col >= matrix[0].len() {
        return EvalResult::Error("REF");
    }
    // Stable sort: apply ascending/descending in the comparator — do not sort then reverse(),
    // which would reverse relative order among ties.
    matrix.sort_by(|a, b| {
        let ord = compare_eval_cells(&a[key_col], &b[key_col]);
        if sort_order < 0 {
            ord.reverse()
        } else {
            ord
        }
    });
    if by_col {
        transpose_matrix(&mut matrix);
    }
    EvalResult::Array(matrix)
}

fn eval_take(
    args: &[Ast],
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
) -> EvalResult {
    if args.len() < 2 || args.len() > 3 {
        return EvalResult::Error("ARGS");
    }
    let matrix =
        match collect_matrix_values(&args[0], grid, visiting, bindings, budget, allow_templates) {
            Ok(m) => m,
            Err(e) => return EvalResult::Error(e),
        };
    slice_take_drop(
        matrix,
        &args[1],
        args.get(2),
        grid,
        visiting,
        bindings,
        budget,
        allow_templates,
        true,
    )
}

fn eval_drop(
    args: &[Ast],
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
) -> EvalResult {
    if args.len() < 2 || args.len() > 3 {
        return EvalResult::Error("ARGS");
    }
    let matrix =
        match collect_matrix_values(&args[0], grid, visiting, bindings, budget, allow_templates) {
            Ok(m) => m,
            Err(e) => return EvalResult::Error(e),
        };
    slice_take_drop(
        matrix,
        &args[1],
        args.get(2),
        grid,
        visiting,
        bindings,
        budget,
        allow_templates,
        false,
    )
}

fn eval_choosecols(
    args: &[Ast],
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
) -> EvalResult {
    if args.len() < 2 {
        return EvalResult::Error("ARGS");
    }
    let matrix =
        match collect_matrix_values(&args[0], grid, visiting, bindings, budget, allow_templates) {
            Ok(m) => m,
            Err(e) => return EvalResult::Error(e),
        };
    choose_axes(
        matrix,
        &args[1..],
        grid,
        visiting,
        bindings,
        budget,
        allow_templates,
        true,
    )
}

fn eval_chooserows(
    args: &[Ast],
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
) -> EvalResult {
    if args.len() < 2 {
        return EvalResult::Error("ARGS");
    }
    let matrix =
        match collect_matrix_values(&args[0], grid, visiting, bindings, budget, allow_templates) {
            Ok(m) => m,
            Err(e) => return EvalResult::Error(e),
        };
    choose_axes(
        matrix,
        &args[1..],
        grid,
        visiting,
        bindings,
        budget,
        allow_templates,
        false,
    )
}

#[optimize(speed)]
fn collect_matrix_values(
    arg: &Ast,
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
) -> Result<Vec<Vec<EvalResult>>, &'static str> {
    match arg {
        Ast::Range {
            range: r,
            locks_tl: _,
            locks_br: _,
        } => {
            let mut out = Vec::new();
            for row in r.row_start..r.row_end {
                let mut out_row = Vec::new();
                for col in r.col_start..r.col_end {
                    let addr = CellAddr::Main { row, col };
                    out_row.push(
                        eval_cell_inner(grid, &addr, visiting, budget, allow_templates)
                            .scalar_coerce(),
                    );
                }
                out.push(out_row);
            }
            Ok(out)
        }
        Ast::Ref { addr, .. } => Ok(vec![vec![eval_cell_inner(
            grid,
            addr,
            visiting,
            budget,
            allow_templates,
        )
        .scalar_coerce()]]),
        _ => match eval_ast(arg, grid, visiting, bindings, budget, allow_templates) {
            EvalResult::Array(rows) => Ok(rows),
            other => Ok(vec![vec![other]]),
        },
    }
}

fn collect_criteria_pairs(
    args: &[Ast],
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
) -> Result<Vec<(MainRange, Criteria)>, &'static str> {
    if args.len() < 2 || !args.len().is_multiple_of(2) {
        return Err("ARGS");
    }
    let mut out = Vec::new();
    for pair in args.chunks(2) {
        let Some(range) = as_main_range(&pair[0]) else {
            return Err("RANGE");
        };
        let criteria =
            criteria_from_ast(&pair[1], grid, visiting, bindings, budget, allow_templates)?;
        out.push((range, criteria));
    }
    let base = out[0].0.clone();
    if out.iter().any(|(r, _)| {
        range_height(r) != range_height(&base) || range_width(r) != range_width(&base)
    }) {
        return Err("ARGS");
    }
    Ok(out)
}

fn range_height(r: &MainRange) -> u32 {
    r.row_end.saturating_sub(r.row_start)
}

fn range_width(r: &MainRange) -> u32 {
    r.col_end.saturating_sub(r.col_start)
}

fn compare_eval_cells(a: &EvalResult, b: &EvalResult) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let a = a.clone().scalar_coerce();
    let b = b.clone().scalar_coerce();
    match (a, b) {
        (EvalResult::Number(x), EvalResult::Number(y)) => {
            x.partial_cmp(&y).unwrap_or(Ordering::Equal)
        }
        (EvalResult::Text(x), EvalResult::Text(y)) => x.cmp(&y),
        (EvalResult::Number(_), EvalResult::Text(_)) => Ordering::Less,
        (EvalResult::Text(_), EvalResult::Number(_)) => Ordering::Greater,
        (EvalResult::Error(x), EvalResult::Error(y)) => x.cmp(y),
        (EvalResult::Error(_), _) => Ordering::Greater,
        (_, EvalResult::Error(_)) => Ordering::Less,
        _ => Ordering::Equal,
    }
}

fn transpose_matrix(matrix: &mut Vec<Vec<EvalResult>>) {
    if matrix.is_empty() {
        return;
    }
    let rows = matrix.len();
    let cols = matrix.iter().map(|r| r.len()).max().unwrap_or(0);
    let mut out = vec![vec![EvalResult::Error("CALC"); rows]; cols];
    for (r, row) in matrix.iter().enumerate() {
        for (c, cell) in row.iter().enumerate() {
            out[c][r] = cell.clone();
        }
    }
    *matrix = out;
}

fn slice_take_drop(
    matrix: Vec<Vec<EvalResult>>,
    row_arg: &Ast,
    col_arg: Option<&Ast>,
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
    take: bool,
) -> EvalResult {
    let rows_n = match numeric_value(eval_ast(
        row_arg,
        grid,
        visiting,
        bindings,
        budget,
        allow_templates,
    )) {
        Some(n) if n.is_finite() => n as isize,
        _ => return EvalResult::Error("VALUE"),
    };
    let cols_n = if let Some(col_arg) = col_arg {
        match numeric_value(eval_ast(
            col_arg,
            grid,
            visiting,
            bindings,
            budget,
            allow_templates,
        )) {
            Some(n) if n.is_finite() => n as isize,
            _ => return EvalResult::Error("VALUE"),
        }
    } else {
        0
    };
    let mut out = matrix;
    if take {
        out = slice_rows(out, rows_n, true);
        if col_arg.is_some() {
            out = slice_cols(out, cols_n, true);
        }
    } else {
        out = slice_rows(out, rows_n, false);
        if col_arg.is_some() {
            out = slice_cols(out, cols_n, false);
        }
    }
    EvalResult::Array(out)
}

fn slice_rows(mut matrix: Vec<Vec<EvalResult>>, n: isize, take: bool) -> Vec<Vec<EvalResult>> {
    if matrix.is_empty() {
        return matrix;
    }
    let len = matrix.len() as isize;
    let n = if n < 0 { len + n } else { n };
    if take {
        if n >= 0 {
            matrix.truncate(n.min(len) as usize);
            matrix
        } else {
            let keep = (len + n).max(0) as usize;
            matrix.into_iter().take(keep).collect()
        }
    } else if n >= 0 {
        matrix.into_iter().skip(n.min(len) as usize).collect()
    } else {
        let keep = (len + n).max(0) as usize;
        matrix.truncate(keep);
        matrix
    }
}

fn slice_cols(matrix: Vec<Vec<EvalResult>>, n: isize, take: bool) -> Vec<Vec<EvalResult>> {
    if matrix.is_empty() {
        return matrix;
    }
    let len = matrix[0].len() as isize;
    let n = if n < 0 { len + n } else { n };
    matrix
        .into_iter()
        .map(|mut row| {
            if take {
                row.truncate(n.min(len) as usize);
                row
            } else if n >= 0 {
                row.into_iter().skip(n.min(len) as usize).collect()
            } else {
                let keep = (len + n).max(0) as usize;
                row.truncate(keep);
                row
            }
        })
        .collect()
}

fn choose_axes(
    matrix: Vec<Vec<EvalResult>>,
    args: &[Ast],
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
    cols: bool,
) -> EvalResult {
    let mut indices = Vec::new();
    for arg in args {
        let idx = match numeric_value(eval_ast(
            arg,
            grid,
            visiting,
            bindings,
            budget,
            allow_templates,
        )) {
            Some(n) if n.is_finite() && n != 0.0 => n as isize,
            _ => return EvalResult::Error("VALUE"),
        };
        indices.push(idx);
    }
    if cols {
        let mut out = Vec::new();
        for row in matrix {
            let mut new_row = Vec::new();
            for idx in &indices {
                let j = resolve_index(*idx, row.len());
                let Some(j) = j else {
                    return EvalResult::Error("REF");
                };
                new_row.push(row[j].clone());
            }
            out.push(new_row);
        }
        EvalResult::Array(out)
    } else {
        let mut out = Vec::new();
        for idx in indices {
            let j = match resolve_index(idx, matrix.len()) {
                Some(v) => v,
                None => return EvalResult::Error("REF"),
            };
            out.push(matrix[j].clone());
        }
        EvalResult::Array(out)
    }
}

fn resolve_index(idx: isize, len: usize) -> Option<usize> {
    if idx > 0 {
        let i = idx as usize - 1;
        (i < len).then_some(i)
    } else if idx < 0 {
        let i = len as isize + idx;
        (i >= 0).then_some(i as usize)
    } else {
        None
    }
}

fn eval_count(
    args: &[Ast],
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
) -> EvalResult {
    if args.len() != 1 {
        return EvalResult::Error("ARGS");
    }
    match count_numeric_values(&args[0], grid, visiting, bindings, budget, allow_templates) {
        Ok(n) => EvalResult::Number(Number::from_i64(n as i64)),
        Err(e) => EvalResult::Error(e),
    }
}

fn eval_counta(
    args: &[Ast],
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
) -> EvalResult {
    if args.len() != 1 {
        return EvalResult::Error("ARGS");
    }
    match count_nonempty_values(&args[0], grid, visiting, bindings, budget, allow_templates) {
        Ok(n) => EvalResult::Number(Number::from_i64(n as i64)),
        Err(e) => EvalResult::Error(e),
    }
}

fn eval_countblank(
    args: &[Ast],
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
) -> EvalResult {
    if args.len() != 1 {
        return EvalResult::Error("ARGS");
    }
    let count = match &args[0] {
        Ast::Range {
            range: r,
            locks_tl: _,
            locks_br: _,
        } => count_blank_range(grid, r, visiting, budget, allow_templates),
        Ast::Ref { addr, .. } => usize::from(cell_is_blank_for_count(
            grid,
            addr,
            visiting,
            budget,
            allow_templates,
        )),
        Ast::SheetRef {
            sheet_id,
            addr,
            locks: _,
        } => {
            let Some(sheet_grid) = super::workbook_lookup(*sheet_id) else {
                return EvalResult::Error("SHEET");
            };
            usize::from(cell_is_blank_for_count(
                &sheet_grid,
                addr,
                &mut Vec::new(),
                budget,
                allow_templates,
            ))
        }
        _ => usize::from(matches!(
            eval_ast(&args[0], grid, visiting, bindings, budget, allow_templates).scalar_coerce(),
            EvalResult::Text(s) if s.is_empty()
        )),
    };
    EvalResult::Number(Number::from_i64(count as i64))
}

fn eval_typeof(
    args: &[Ast],
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
) -> EvalResult {
    if args.len() != 1 {
        return EvalResult::Error("ARGS");
    }
    if matches_blank_ref(
        &args[0],
        grid,
        visiting,
        bindings,
        budget,
        allow_templates,
    ) {
        return EvalResult::Text("blank".into());
    }
    match eval_ast(
        &args[0],
        grid,
        visiting,
        bindings,
        budget,
        allow_templates,
    )
    .scalar_coerce()
    {
        EvalResult::Bool(_) => EvalResult::Text("bool".into()),
        EvalResult::Number(Number::Exact(_)) => EvalResult::Text("exact".into()),
        EvalResult::Number(Number::Approx(_)) => EvalResult::Text("approx".into()),
        EvalResult::Number(Number::Complex(_)) => EvalResult::Text("complex".into()),
        EvalResult::Text(s) if s.is_empty() => EvalResult::Text("blank".into()),
        EvalResult::Text(_) => EvalResult::Text("text".into()),
        EvalResult::Error(_) => EvalResult::Text("error".into()),
        EvalResult::Array(_) => EvalResult::Text("array".into()),
    }
}

fn eval_type(
    args: &[Ast],
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
) -> EvalResult {
    // LibreOffice TYPE compatibility: return numeric codes
    // Number -> 1, Text -> 2, Logical -> 4, Formula -> 8, Error -> 16, Array -> 64
    if args.len() != 1 {
        return EvalResult::Error("ARGS");
    }

    // If the argument is an explicit range AST, it's an array
    if matches!(args[0], Ast::Range { .. }) {
        return EvalResult::Number(Number::from_i64(64));
    }

    // If the argument is a direct cell reference, prefer detecting a stored
    // formula (leading '=' in the stored cell text) and return 8 in that case
    match &args[0] {
        Ast::Ref { addr, .. } => {
            if let Some(raw) = grid.get(addr) {
                if raw.trim().starts_with('=') {
                    return EvalResult::Number(Number::from_i64(8));
                }
            }
        }
        Ast::SheetRef { sheet_id, addr, .. } => {
            if let Some(sheet_grid) = super::workbook_lookup(*sheet_id) {
                if let Some(raw) = sheet_grid.get(addr) {
                    if raw.trim().starts_with('=') {
                        return EvalResult::Number(Number::from_i64(8));
                    }
                }
            }
        }
        _ => {}
    }

    // Fall back to evaluating the argument and mapping the runtime type to
    // the numeric code.
    match eval_ast(
        &args[0],
        grid,
        visiting,
        bindings,
        budget,
        allow_templates,
    )
    .scalar_coerce()
    {
        EvalResult::Number(_) => EvalResult::Number(Number::from_i64(1)),
        EvalResult::Text(_) => EvalResult::Number(Number::from_i64(2)),
        EvalResult::Bool(_) => EvalResult::Number(Number::from_i64(4)),
        EvalResult::Error(_) => EvalResult::Number(Number::from_i64(16)),
        EvalResult::Array(_) => EvalResult::Number(Number::from_i64(64)),
    }
}

fn eval_isnumber(
    args: &[Ast],
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
) -> EvalResult {
    unary_is_pred(
        args,
        grid,
        visiting,
        bindings,
        budget,
        allow_templates,
        |v| matches!(v.scalar_coerce(), EvalResult::Number(_)),
    )
}

fn eval_istext(
    args: &[Ast],
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
) -> EvalResult {
    unary_is_pred(
        args,
        grid,
        visiting,
        bindings,
        budget,
        allow_templates,
        |v| matches!(v.scalar_coerce(), EvalResult::Text(_)),
    )
}

fn eval_isblank(
    args: &[Ast],
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
) -> EvalResult {
    if args.len() != 1 {
        return EvalResult::Error("ARGS");
    }
    EvalResult::Number(if matches_blank_ref(&args[0], grid, visiting, bindings, budget, allow_templates)
    {
        Number::one()
    } else {
        Number::exact_zero()
    })
}

fn eval_iserror(
    args: &[Ast],
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
) -> EvalResult {
    unary_is_pred(
        args,
        grid,
        visiting,
        bindings,
        budget,
        allow_templates,
        |v| matches!(v.scalar_coerce(), EvalResult::Error(_)),
    )
}

fn eval_isna(
    args: &[Ast],
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
) -> EvalResult {
    unary_is_pred(
        args,
        grid,
        visiting,
        bindings,
        budget,
        allow_templates,
        |v| matches!(v.scalar_coerce(), EvalResult::Error("NA")),
    )
}

fn unary_is_pred<F>(
    args: &[Ast],
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
    pred: F,
) -> EvalResult
where
    F: Fn(EvalResult) -> bool,
{
    if args.len() != 1 {
        return EvalResult::Error("ARGS");
    }
    let p = pred(eval_ast(
        &args[0],
        grid,
        visiting,
        bindings,
        budget,
        allow_templates,
    ));
    EvalResult::Number(Number::from_i64(if p { 1 } else { 0 }))
}

fn matches_blank_ref(
    ast: &Ast,
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
) -> bool {
    match ast {
        Ast::Ref { addr, .. } => cell_is_blank_raw(grid, addr),
        Ast::SheetRef {
            sheet_id,
            addr,
            locks: _,
        } => {
            let Some(sheet_grid) = super::workbook_lookup(*sheet_id) else {
                return false;
            };
            cell_is_blank_raw(&sheet_grid, addr)
        }
        _ => matches!(
            eval_ast(ast, grid, visiting, bindings, budget, allow_templates).scalar_coerce(),
            EvalResult::Text(s) if s.is_empty()
        ),
    }
}

fn cell_is_blank_raw(grid: &Grid, addr: &CellAddr) -> bool {
    match grid.get(addr) {
        None => true,
        Some(raw) => raw.trim().is_empty(),
    }
}

fn cell_is_blank_for_count(
    grid: &Grid,
    addr: &CellAddr,
    visiting: &mut Vec<CellAddr>,
    budget: &mut usize,
    allow_templates: bool,
) -> bool {
    match grid.get(addr) {
        None => true,
        Some(raw) => {
            let t = raw.trim();
            if t.is_empty() {
                true
            } else if t.starts_with('=') {
                matches!(
                    eval_cell_inner(grid, addr, visiting, budget, allow_templates).scalar_coerce(),
                    EvalResult::Text(s) if s.is_empty()
                )
            } else {
                false
            }
        }
    }
}

fn count_blank_range(
    grid: &Grid,
    range: &MainRange,
    visiting: &mut Vec<CellAddr>,
    budget: &mut usize,
    allow_templates: bool,
) -> usize {
    let mut count = 0usize;
    for r in range.row_start..range.row_end {
        for c in range.col_start..range.col_end {
            let addr = CellAddr::Main { row: r, col: c };
            if cell_is_blank_for_count(grid, &addr, visiting, budget, allow_templates) {
                count += 1;
            }
        }
    }
    count
}

#[optimize(speed)]
fn eval_countif(
    args: &[Ast],
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
) -> EvalResult {
    if args.len() != 2 {
        return EvalResult::Error("ARGS");
    }
    let Some(range) = as_main_range(&args[0]) else {
        return EvalResult::Error("RANGE");
    };
    let Ok(criteria) =
        criteria_from_ast(&args[1], grid, visiting, bindings, budget, allow_templates)
    else {
        return EvalResult::Error("VALUE");
    };
    let mut count = 0usize;
    for r in range.row_start..range.row_end {
        for c in range.col_start..range.col_end {
            let addr = CellAddr::Main { row: r, col: c };
            if criteria_matches(&criteria, grid, &addr, visiting, budget, allow_templates) {
                count += 1;
            }
        }
    }
    EvalResult::Number(Number::from_i64(count as i64))
}

#[optimize(speed)]
fn eval_sumif(
    args: &[Ast],
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
) -> EvalResult {
    if args.len() != 2 && args.len() != 3 {
        return EvalResult::Error("ARGS");
    }
    let Some(criteria_range) = as_main_range(&args[0]) else {
        return EvalResult::Error("RANGE");
    };
    let sum_range = if args.len() == 3 {
        match as_main_range(&args[2]) {
            Some(r) => r,
            None => return EvalResult::Error("RANGE"),
        }
    } else {
        criteria_range.clone()
    };
    let Ok(criteria) =
        criteria_from_ast(&args[1], grid, visiting, bindings, budget, allow_templates)
    else {
        return EvalResult::Error("VALUE");
    };
    let criteria_rows = criteria_range.row_end - criteria_range.row_start;
    let criteria_cols = criteria_range.col_end - criteria_range.col_start;
    let sum_rows = sum_range.row_end - sum_range.row_start;
    let sum_cols = sum_range.col_end - sum_range.col_start;
    if criteria_rows != sum_rows || criteria_cols != sum_cols {
        return EvalResult::Error("ARGS");
    }
    let mut sum = Number::exact_zero();
    for dr in 0..criteria_rows {
        for dc in 0..criteria_cols {
            let crit_addr = CellAddr::Main {
                row: criteria_range.row_start + dr,
                col: criteria_range.col_start + dc,
            };
            if criteria_matches(
                &criteria,
                grid,
                &crit_addr,
                visiting,
                budget,
                allow_templates,
            ) {
                let sum_addr = CellAddr::Main {
                    row: sum_range.row_start + dr,
                    col: sum_range.col_start + dc,
                };
                if let Some(n) = super::summable_numeric(grid, &sum_addr, visiting, budget) {
                    sum = sum.add(n);
                }
            }
        }
    }
    EvalResult::Number(sum)
}

#[optimize(speed)]
fn collect_numeric_values(
    arg: &Ast,
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
) -> Result<Vec<Number>, &'static str> {
    match arg {
        Ast::Range {
            range: r,
            locks_tl: _,
            locks_br: _,
        } => {
            let mut out = Vec::new();
            for row in r.row_start..r.row_end {
                for col in r.col_start..r.col_end {
                    let addr = CellAddr::Main { row, col };
                    if let Some(n) = super::summable_numeric(grid, &addr, visiting, budget) {
                        out.push(n);
                    }
                }
            }
            Ok(out)
        }
        Ast::Ref { addr, .. } => Ok(super::summable_numeric(grid, addr, visiting, budget)
            .into_iter()
            .collect()),
        _ => match eval_ast(arg, grid, visiting, bindings, budget, allow_templates).scalar_coerce()
        {
            EvalResult::Number(n) => Ok(vec![n]),
            EvalResult::Bool(b) => Ok(vec![number_from_bool(b)]),
            EvalResult::Text(s) => Ok(parse_number_literal(&s).into_iter().collect()),
            EvalResult::Error(e) => Err(e),
            EvalResult::Array(_) => Err("CALC"),
        },
    }
}

fn trim_spaces(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn numeric_value(result: EvalResult) -> Option<f64> {
    match result {
        EvalResult::Number(n) => Some(n.to_f64()),
        EvalResult::Bool(b) => Some(if b { 1.0 } else { 0.0 }),
        EvalResult::Text(s) => parse_numeric_or_date_literal(&s).map(|n| n.to_f64()),
        EvalResult::Error(_) => None,
        EvalResult::Array(_) => None,
    }
}

fn as_main_range(ast: &Ast) -> Option<MainRange> {
    match ast {
        Ast::Range { range, .. } => Some(range.clone()),
        _ => None,
    }
}

#[optimize(speed)]
fn count_numeric_values(
    arg: &Ast,
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
) -> Result<usize, &'static str> {
    match arg {
        Ast::Range {
            range: r,
            locks_tl: _,
            locks_br: _,
        } => {
            let mut n = 0usize;
            for row in r.row_start..r.row_end {
                for col in r.col_start..r.col_end {
                    let addr = CellAddr::Main { row, col };
                    if super::effective_numeric(grid, &addr, visiting, budget).is_some() {
                        n += 1;
                    }
                }
            }
            Ok(n)
        }
        Ast::Ref { addr, .. } => Ok(
            if super::effective_numeric(grid, addr, visiting, budget).is_some() {
                1
            } else {
                0
            },
        ),
        _ => match eval_ast(arg, grid, visiting, bindings, budget, allow_templates).scalar_coerce()
        {
            EvalResult::Number(n) => Ok((!n.is_nan()) as usize),
            EvalResult::Bool(_) => Ok(0),
            EvalResult::Text(s) => Ok(parse_number_literal(&s).is_some() as usize),
            EvalResult::Error(e) => Err(e),
            EvalResult::Array(_) => Err("CALC"),
        },
    }
}

#[optimize(speed)]
fn count_nonempty_values(
    arg: &Ast,
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
) -> Result<usize, &'static str> {
    match arg {
        Ast::Range {
            range: r,
            locks_tl: _,
            locks_br: _,
        } => {
            let mut n = 0usize;
            for row in r.row_start..r.row_end {
                for col in r.col_start..r.col_end {
                    let addr = CellAddr::Main { row, col };
                    if grid.get(&addr).map(|s| !s.is_empty()).unwrap_or(false) {
                        n += 1;
                    }
                }
            }
            Ok(n)
        }
        Ast::Ref { addr, .. } => Ok(grid.get(addr).map(|s| !s.is_empty()).unwrap_or(false) as usize),
        _ => match eval_ast(arg, grid, visiting, bindings, budget, allow_templates).scalar_coerce()
        {
            EvalResult::Number(_) => Ok(1),
            EvalResult::Bool(_) => Ok(1),
            EvalResult::Text(s) => Ok((!s.is_empty()) as usize),
            EvalResult::Error(e) => Err(e),
            EvalResult::Array(_) => Err("CALC"),
        },
    }
}

#[derive(Clone, Copy)]
enum CriteriaOp {
    Eq,
    Ne,
    Gt,
    Ge,
    Lt,
    Le,
}

struct Criteria {
    op: CriteriaOp,
    value: String,
    numeric: Option<f64>,
}

fn compare_f64(op: CriteriaOp, left: f64, right: f64) -> bool {
    match op {
        CriteriaOp::Eq => left == right,
        CriteriaOp::Ne => left != right,
        CriteriaOp::Gt => left > right,
        CriteriaOp::Ge => left >= right,
        CriteriaOp::Lt => left < right,
        CriteriaOp::Le => left <= right,
    }
}

fn compare_str(op: CriteriaOp, left: &str, right: &str) -> bool {
    match op {
        CriteriaOp::Eq => left == right,
        CriteriaOp::Ne => left != right,
        CriteriaOp::Gt => left > right,
        CriteriaOp::Ge => left >= right,
        CriteriaOp::Lt => left < right,
        CriteriaOp::Le => left <= right,
    }
}

#[optimize(speed)]
fn criteria_from_ast(
    ast: &Ast,
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
) -> Result<Criteria, &'static str> {
    let raw = match eval_ast(ast, grid, visiting, bindings, budget, allow_templates).scalar_coerce()
    {
        EvalResult::Number(n) => n.to_string(),
        EvalResult::Bool(true) => "TRUE".into(),
        EvalResult::Bool(false) => "FALSE".into(),
        EvalResult::Text(s) => s,
        EvalResult::Error(e) => return Err(e),
        EvalResult::Array(_) => return Err("CALC"),
    };
    let s = raw.trim();
    let (op, rest) = if let Some(rest) = s.strip_prefix(">=") {
        (CriteriaOp::Ge, rest)
    } else if let Some(rest) = s.strip_prefix("<=") {
        (CriteriaOp::Le, rest)
    } else if let Some(rest) = s.strip_prefix("<>") {
        (CriteriaOp::Ne, rest)
    } else if let Some(rest) = s.strip_prefix('>') {
        (CriteriaOp::Gt, rest)
    } else if let Some(rest) = s.strip_prefix('<') {
        (CriteriaOp::Lt, rest)
    } else if let Some(rest) = s.strip_prefix('=') {
        (CriteriaOp::Eq, rest)
    } else {
        (CriteriaOp::Eq, s)
    };
    Ok(Criteria {
        op,
        numeric: parse_number_literal(rest).map(|n| n.to_f64()),
        value: rest.to_string(),
    })
}

#[optimize(speed)]
fn criteria_matches(
    criteria: &Criteria,
    grid: &Grid,
    addr: &CellAddr,
    visiting: &mut Vec<CellAddr>,
    budget: &mut usize,
    allow_templates: bool,
) -> bool {
    match eval_cell_inner(grid, addr, visiting, budget, allow_templates).scalar_coerce() {
        EvalResult::Number(n) => match criteria.numeric {
            Some(target) => compare_f64(criteria.op, n.to_f64(), target),
            None => compare_str(criteria.op, &n.to_string(), &criteria.value),
        },
        EvalResult::Bool(b) => {
            let n = number_from_bool(b);
            match criteria.numeric {
                Some(target) => compare_f64(criteria.op, n.to_f64(), target),
                None => compare_str(
                    criteria.op,
                    if b { "TRUE" } else { "FALSE" },
                    &criteria.value,
                ),
            }
        }
        EvalResult::Text(s) => match criteria.numeric {
            Some(target) => parse_number_literal(&s)
                .map(|num| compare_f64(criteria.op, num.to_f64(), target))
                .unwrap_or(false),
            None => compare_str(criteria.op, &s, &criteria.value),
        },
        EvalResult::Error(_) => false,
        EvalResult::Array(_) => false,
    }
}

#[derive(Clone, Debug, PartialEq)]
enum LookupValue {
    Number(Number),
    Text(String),
}

fn eval_lookup(
    args: &[Ast],
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
) -> EvalResult {
    if args.len() != 2 && args.len() != 3 {
        return EvalResult::Error("ARGS");
    }
    let lookup_value = match eval_ast(&args[0], grid, visiting, bindings, budget, allow_templates)
        .scalar_coerce()
    {
        EvalResult::Number(n) => LookupValue::Number(n),
        EvalResult::Bool(b) => LookupValue::Number(number_from_bool(b)),
        EvalResult::Text(s) => parse_number_literal(&s)
            .map(LookupValue::Number)
            .unwrap_or(LookupValue::Text(s)),
        EvalResult::Error(e) => return EvalResult::Error(e),
        EvalResult::Array(_) => return EvalResult::Error("CALC"),
    };
    let Some(lookup_range) = as_main_range(&args[1]) else {
        return EvalResult::Error("RANGE");
    };
    let result_range = if args.len() == 3 {
        match as_main_range(&args[2]) {
            Some(r) => r,
            None => return EvalResult::Error("RANGE"),
        }
    } else {
        lookup_range.clone()
    };

    let lookup_rows = lookup_range.row_end - lookup_range.row_start;
    let lookup_cols = lookup_range.col_end - lookup_range.col_start;
    let result_rows = result_range.row_end - result_range.row_start;
    let result_cols = result_range.col_end - result_range.col_start;
    if lookup_rows != result_rows || lookup_cols != result_cols {
        return EvalResult::Error("ARGS");
    }
    if !(lookup_rows == 1 || lookup_cols == 1) {
        return EvalResult::Error("ARGS");
    }

    let len = if lookup_rows == 1 {
        lookup_cols
    } else {
        lookup_rows
    };
    for i in 0..len {
        let lookup_addr = if lookup_rows == 1 {
            CellAddr::Main {
                row: lookup_range.row_start,
                col: lookup_range.col_start + i,
            }
        } else {
            CellAddr::Main {
                row: lookup_range.row_start + i,
                col: lookup_range.col_start,
            }
        };
        let candidate = match eval_cell_inner(grid, &lookup_addr, visiting, budget, allow_templates)
            .scalar_coerce()
        {
            EvalResult::Number(n) => LookupValue::Number(n),
            EvalResult::Bool(b) => LookupValue::Number(number_from_bool(b)),
            EvalResult::Text(s) => parse_number_literal(&s)
                .map(LookupValue::Number)
                .unwrap_or(LookupValue::Text(s)),
            EvalResult::Error(_) => continue,
            EvalResult::Array(_) => continue,
        };
        if lookup_value == candidate {
            let result_addr = if result_rows == 1 {
                CellAddr::Main {
                    row: result_range.row_start,
                    col: result_range.col_start + i,
                }
            } else {
                CellAddr::Main {
                    row: result_range.row_start + i,
                    col: result_range.col_start,
                }
            };
            return eval_cell_inner(grid, &result_addr, visiting, budget, allow_templates);
        }
    }
    EvalResult::Error("NA")
}

fn eval_vlookup(
    args: &[Ast],
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
) -> EvalResult {
    if args.len() != 3 && args.len() != 4 {
        return EvalResult::Error("ARGS");
    }
    let lookup_value = match eval_ast(&args[0], grid, visiting, bindings, budget, allow_templates)
        .scalar_coerce()
    {
        EvalResult::Number(n) => LookupValue::Number(n),
        EvalResult::Bool(b) => LookupValue::Number(number_from_bool(b)),
        EvalResult::Text(s) => parse_number_literal(&s)
            .map(LookupValue::Number)
            .unwrap_or(LookupValue::Text(s)),
        EvalResult::Error(e) => return EvalResult::Error(e),
        EvalResult::Array(_) => return EvalResult::Error("CALC"),
    };
    let Some(table) = as_main_range(&args[1]) else {
        return EvalResult::Error("RANGE");
    };
    let col_index = match eval_ast(&args[2], grid, visiting, bindings, budget, allow_templates) {
        EvalResult::Number(n) => {
            let nf = n.to_f64();
            if nf.is_finite() && nf >= 1.0 && nf.fract() == 0.0 {
                nf as u32
            } else {
                return EvalResult::Error("VALUE");
            }
        }
        EvalResult::Text(s) => match parse_number_literal(&s) {
            Some(n) => {
                let nf = n.to_f64();
                if nf.is_finite() && nf >= 1.0 && nf.fract() == 0.0 {
                    nf as u32
                } else {
                    return EvalResult::Error("VALUE");
                }
            }
            None => return EvalResult::Error("VALUE"),
        },
        EvalResult::Error(e) => return EvalResult::Error(e),
        _ => return EvalResult::Error("VALUE"),
    };
    let table_rows = table.row_end - table.row_start;
    let table_cols = table.col_end - table.col_start;
    if col_index == 0 || col_index > table_cols {
        return EvalResult::Error("REF");
    }
    for dr in 0..table_rows {
        let key_addr = CellAddr::Main {
            row: table.row_start + dr,
            col: table.col_start,
        };
        let candidate = match eval_cell_inner(grid, &key_addr, visiting, budget, allow_templates)
            .scalar_coerce()
        {
            EvalResult::Number(n) => LookupValue::Number(n),
            EvalResult::Bool(b) => LookupValue::Number(number_from_bool(b)),
            EvalResult::Text(s) => parse_number_literal(&s)
                .map(LookupValue::Number)
                .unwrap_or(LookupValue::Text(s)),
            EvalResult::Error(_) => continue,
            EvalResult::Array(_) => continue,
        };
        if lookup_value == candidate {
            let result_addr = CellAddr::Main {
                row: table.row_start + dr,
                col: table.col_start + (col_index - 1),
            };
            return eval_cell_inner(grid, &result_addr, visiting, budget, allow_templates);
        }
    }
    EvalResult::Error("NA")
}

fn eval_xlookup(
    args: &[Ast],
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
) -> EvalResult {
    if args.len() != 3 && args.len() != 4 {
        return EvalResult::Error("ARGS");
    }
    let lookup_value = match eval_ast(&args[0], grid, visiting, bindings, budget, allow_templates)
        .scalar_coerce()
    {
        EvalResult::Number(n) => LookupValue::Number(n),
        EvalResult::Bool(b) => LookupValue::Number(number_from_bool(b)),
        EvalResult::Text(s) => parse_number_literal(&s)
            .map(LookupValue::Number)
            .unwrap_or(LookupValue::Text(s)),
        EvalResult::Error(e) => return EvalResult::Error(e),
        EvalResult::Array(_) => return EvalResult::Error("CALC"),
    };
    let Some(lookup_range) = as_main_range(&args[1]) else {
        return EvalResult::Error("RANGE");
    };
    let Some(return_range) = as_main_range(&args[2]) else {
        return EvalResult::Error("RANGE");
    };

    let lookup_rows = lookup_range.row_end - lookup_range.row_start;
    let lookup_cols = lookup_range.col_end - lookup_range.col_start;
    let return_rows = return_range.row_end - return_range.row_start;
    let return_cols = return_range.col_end - return_range.col_start;
    if lookup_rows != return_rows || lookup_cols != return_cols {
        return EvalResult::Error("ARGS");
    }
    if !(lookup_rows == 1 || lookup_cols == 1) {
        return EvalResult::Error("ARGS");
    }

    let if_not_found = if args.len() == 4 {
        Some(eval_ast(
            &args[3],
            grid,
            visiting,
            bindings,
            budget,
            allow_templates,
        ))
    } else {
        None
    };

    let len = if lookup_rows == 1 {
        lookup_cols
    } else {
        lookup_rows
    };
    for i in 0..len {
        let lookup_addr = if lookup_rows == 1 {
            CellAddr::Main {
                row: lookup_range.row_start,
                col: lookup_range.col_start + i,
            }
        } else {
            CellAddr::Main {
                row: lookup_range.row_start + i,
                col: lookup_range.col_start,
            }
        };
        let candidate = match eval_cell_inner(grid, &lookup_addr, visiting, budget, allow_templates)
            .scalar_coerce()
        {
            EvalResult::Number(n) => LookupValue::Number(n),
            EvalResult::Bool(b) => LookupValue::Number(number_from_bool(b)),
            EvalResult::Text(s) => parse_number_literal(&s)
                .map(LookupValue::Number)
                .unwrap_or(LookupValue::Text(s)),
            EvalResult::Error(_) => continue,
            EvalResult::Array(_) => continue,
        };
        if lookup_value == candidate {
            let result_addr = if return_rows == 1 {
                CellAddr::Main {
                    row: return_range.row_start,
                    col: return_range.col_start + i,
                }
            } else {
                CellAddr::Main {
                    row: return_range.row_start + i,
                    col: return_range.col_start,
                }
            };
            return eval_cell_inner(grid, &result_addr, visiting, budget, allow_templates);
        }
    }

    if let Some(v) = if_not_found {
        return v;
    }
    EvalResult::Error("NA")
}

fn eval_let(
    args: &[Ast],
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
) -> EvalResult {
    if args.len() < 3 || args.len().is_multiple_of(2) {
        return EvalResult::Error("ARGS");
    }
    let base_len = bindings.len();
    let mut i = 0usize;
    while i + 1 < args.len() {
        let name = match &args[i] {
            Ast::Name(s) => s.clone(),
            _ => {
                bindings.truncate(base_len);
                return EvalResult::Error("NAME");
            }
        };
        let value = eval_ast(
            &args[i + 1],
            grid,
            visiting,
            bindings,
            budget,
            allow_templates,
        );
        if matches!(value, EvalResult::Error(_)) {
            bindings.truncate(base_len);
            return value;
        }
        bindings.push((name, value));
        i += 2;
    }
    let result = eval_ast(
        &args[args.len() - 1],
        grid,
        visiting,
        bindings,
        budget,
        allow_templates,
    );
    bindings.truncate(base_len);
    result
}

fn eval_choose(
    args: &[Ast],
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
) -> EvalResult {
    if args.len() < 2 {
        return EvalResult::Error("ARGS");
    }
    let index = match numeric_value(eval_ast(
        &args[0],
        grid,
        visiting,
        bindings,
        budget,
        allow_templates,
    )) {
        Some(n) if n >= 1.0 => n as usize,
        _ => return EvalResult::Error("VALUE"),
    };
    let choice = args.get(index);
    match choice {
        Some(ast) => eval_ast(ast, grid, visiting, bindings, budget, allow_templates),
        None => EvalResult::Error("VALUE"),
    }
}

fn eval_switch(
    args: &[Ast],
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
) -> EvalResult {
    if args.len() < 3 {
        return EvalResult::Error("ARGS");
    }
    let expr =
        eval_ast(&args[0], grid, visiting, bindings, budget, allow_templates).scalar_coerce();
    let mut i = 1usize;
    while i + 1 < args.len() {
        let case =
            eval_ast(&args[i], grid, visiting, bindings, budget, allow_templates).scalar_coerce();
        if case == expr {
            return eval_ast(
                &args[i + 1],
                grid,
                visiting,
                bindings,
                budget,
                allow_templates,
            );
        }
        i += 2;
    }
    if args.len().is_multiple_of(2) {
        eval_ast(
            &args[args.len() - 1],
            grid,
            visiting,
            bindings,
            budget,
            allow_templates,
        )
    } else {
        EvalResult::Error("NA")
    }
}

fn eval_sortby(
    args: &[Ast],
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
) -> EvalResult {
    if args.len() < 3 || args.len().is_multiple_of(2) {
        return EvalResult::Error("ARGS");
    }
    let matrix =
        match collect_matrix_values(&args[0], grid, visiting, bindings, budget, allow_templates) {
            Ok(m) => m,
            Err(e) => return EvalResult::Error(e),
        };
    let mut keys = Vec::new();
    for pair in args[1..].chunks(2) {
        let key_matrix = match collect_matrix_values(
            &pair[0],
            grid,
            visiting,
            bindings,
            budget,
            allow_templates,
        ) {
            Ok(m) => m,
            Err(e) => return EvalResult::Error(e),
        };
        let order = match numeric_value(eval_ast(
            &pair[1],
            grid,
            visiting,
            bindings,
            budget,
            allow_templates,
        )) {
            Some(n) if n < 0.0 => -1,
            Some(_) => 1,
            None => return EvalResult::Error("VALUE"),
        };
        keys.push((key_matrix, order));
    }
    if matrix.is_empty() {
        return EvalResult::Array(matrix);
    }
    let row_count = matrix.len();
    if keys.iter().any(|(k, _)| k.len() != row_count) {
        return EvalResult::Error("ARGS");
    }
    let mut idxs: Vec<usize> = (0..row_count).collect();
    idxs.sort_by(|&a, &b| {
        for (key_matrix, order) in &keys {
            let ord = compare_eval_cells(&key_matrix[a][0], &key_matrix[b][0]);
            if ord != std::cmp::Ordering::Equal {
                return if *order < 0 { ord.reverse() } else { ord };
            }
        }
        std::cmp::Ordering::Equal
    });
    let sorted = idxs.into_iter().map(|i| matrix[i].clone()).collect();
    EvalResult::Array(sorted)
}

fn eval_sequence(
    args: &[Ast],
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
) -> EvalResult {
    if args.is_empty() || args.len() > 4 {
        return EvalResult::Error("ARGS");
    }
    let rows = match eval_ast(&args[0], grid, visiting, bindings, budget, allow_templates)
        .scalar_coerce()
    {
        EvalResult::Number(n) if n.is_finite() && n.to_f64() >= 0.0 => n.to_f64() as usize,
        _ => return EvalResult::Error("VALUE"),
    };
    let cols = if args.len() >= 2 {
        match eval_ast(&args[1], grid, visiting, bindings, budget, allow_templates).scalar_coerce()
        {
            EvalResult::Number(n) if n.is_finite() && n.to_f64() >= 0.0 => n.to_f64() as usize,
            _ => return EvalResult::Error("VALUE"),
        }
    } else {
        1
    };
    let start = if args.len() >= 3 {
        match eval_ast(&args[2], grid, visiting, bindings, budget, allow_templates).scalar_coerce()
        {
            EvalResult::Number(n) => n,
            _ => return EvalResult::Error("VALUE"),
        }
    } else {
        Number::one()
    };
    let step = if args.len() >= 4 {
        match eval_ast(&args[3], grid, visiting, bindings, budget, allow_templates).scalar_coerce()
        {
            EvalResult::Number(n) => n,
            _ => return EvalResult::Error("VALUE"),
        }
    } else {
        Number::one()
    };
    let mut out = Vec::with_capacity(rows);
    for r in 0..rows {
        let mut row = Vec::with_capacity(cols);
        for c in 0..cols {
            let k = Number::from_i64((r * cols + c) as i64);
            row.push(EvalResult::Number(
                start.clone().add(step.clone().mul(k)),
            ));
        }
        out.push(row);
    }
    EvalResult::Array(out)
}

fn eval_unique(
    args: &[Ast],
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
) -> EvalResult {
    if args.len() != 1 {
        return EvalResult::Error("ARGS");
    }
    let values =
        match collect_array_values(&args[0], grid, visiting, bindings, budget, allow_templates) {
            Ok(v) => v,
            Err(e) => return EvalResult::Error(e),
        };
    let mut seen = Vec::<String>::new();
    let mut out = Vec::new();
    for v in values {
        let key = eval_result_to_key(&v);
        if !seen.contains(&key) {
            seen.push(key);
            out.push(vec![v]);
        }
    }
    EvalResult::Array(out)
}

fn eval_filter(
    args: &[Ast],
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
) -> EvalResult {
    if args.len() != 2 {
        return EvalResult::Error("ARGS");
    }
    let values =
        match collect_array_values(&args[0], grid, visiting, bindings, budget, allow_templates) {
            Ok(v) => v,
            Err(e) => return EvalResult::Error(e),
        };
    let mask =
        match collect_array_values(&args[1], grid, visiting, bindings, budget, allow_templates) {
            Ok(v) => v,
            Err(e) => return EvalResult::Error(e),
        };
    let mut out = Vec::new();
    for (idx, v) in values.into_iter().enumerate() {
        let keep = mask
            .get(idx)
            .and_then(|cell| cell.top_left())
            .cloned()
            .map(super::truthy)
            .unwrap_or(false);
        if keep {
            out.push(vec![v]);
        }
    }
    EvalResult::Array(out)
}

fn collect_array_values(
    arg: &Ast,
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
) -> Result<Vec<EvalResult>, &'static str> {
    match arg {
        Ast::Range {
            range: r,
            locks_tl: _,
            locks_br: _,
        } => {
            let mut out = Vec::new();
            for row in r.row_start..r.row_end {
                for col in r.col_start..r.col_end {
                    let addr = CellAddr::Main { row, col };
                    out.push(eval_cell_inner(
                        grid,
                        &addr,
                        visiting,
                        budget,
                        allow_templates,
                    ));
                }
            }
            Ok(out)
        }
        Ast::Ref { addr, .. } => Ok(vec![eval_cell_inner(
            grid,
            addr,
            visiting,
            budget,
            allow_templates,
        )]),
        _ => Ok(vec![eval_ast(
            arg,
            grid,
            visiting,
            bindings,
            budget,
            allow_templates,
        )]),
    }
}

fn eval_result_to_key(result: &EvalResult) -> String {
    match result {
        EvalResult::Number(n) => n.to_string(),
        EvalResult::Bool(true) => "TRUE".into(),
        EvalResult::Bool(false) => "FALSE".into(),
        EvalResult::Text(s) => s.clone(),
        EvalResult::Error(e) => format!("#{e}"),
        EvalResult::Array(rows) => rows
            .first()
            .and_then(|row| row.first())
            .map(eval_result_to_key)
            .unwrap_or_else(|| "#CALC".to_string()),
    }
}

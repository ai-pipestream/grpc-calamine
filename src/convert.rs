// SPDX-License-Identifier: Apache-2.0

//! Conversions between `calamine` types and the `calamine.v1` protobuf
//! messages.
//!
//! Every function here is a pure, total mapping; the protobuf model was
//! designed one-to-one against calamine, so nothing is lossy by construction.

use calamine::{CellErrorType, Data, DataRef, ExcelDateTime, Sheet, SheetVisible};

use crate::proto::v1 as pb;

/// Map `calamine::CellErrorType` onto `CellErrorType`.
#[must_use]
pub fn cell_error_type(e: &CellErrorType) -> pb::CellErrorType {
    match e {
        CellErrorType::Div0 => pb::CellErrorType::Div0,
        CellErrorType::NA => pb::CellErrorType::Na,
        CellErrorType::Name => pb::CellErrorType::Name,
        CellErrorType::Null => pb::CellErrorType::Null,
        CellErrorType::Num => pb::CellErrorType::Num,
        CellErrorType::Ref => pb::CellErrorType::Ref,
        CellErrorType::Value => pb::CellErrorType::Value,
        CellErrorType::GettingData => pb::CellErrorType::GettingData,
    }
}

/// Map `calamine::ExcelDateTime` onto `ExcelDateTime`.
///
/// The 1904 flag is a workbook-level property, not a cell-level one, so it
/// is read once per workbook via the readers' `has_1904_epoch` methods and
/// passed in here rather than derived from the value.
#[must_use]
pub fn excel_date_time(dt: &ExcelDateTime, is_1904: bool) -> pb::ExcelDateTime {
    let datetime_type = if dt.is_duration() {
        pb::ExcelDateTimeType::TimeDelta
    } else {
        pb::ExcelDateTimeType::DateTime
    };
    pb::ExcelDateTime {
        value: dt.as_f64(),
        datetime_type: datetime_type as i32,
        is_1904,
    }
}

/// Map an owned `calamine::Data` onto the `CellData.value` oneof.
///
/// `is_1904` is the workbook's date-system flag, stamped onto every
/// datetime cell.
#[must_use]
pub fn data_value(d: &Data, is_1904: bool) -> pb::cell_data::Value {
    match d {
        Data::Int(v) => pb::cell_data::Value::IntValue(*v),
        Data::Float(v) => pb::cell_data::Value::FloatValue(*v),
        Data::String(v) => pb::cell_data::Value::StringValue(v.clone()),
        Data::Bool(v) => pb::cell_data::Value::BoolValue(*v),
        Data::DateTime(v) => pb::cell_data::Value::DateTime(excel_date_time(v, is_1904)),
        Data::DateTimeIso(v) => pb::cell_data::Value::DateTimeIso(v.clone()),
        Data::DurationIso(v) => pb::cell_data::Value::DurationIso(v.clone()),
        Data::Error(e) => pb::cell_data::Value::Error(cell_error_type(e) as i32),
        Data::Empty => pb::cell_data::Value::Empty(()),
    }
}

/// Map a borrowed `calamine::DataRef` onto the `CellData.value` oneof.
///
/// `DataRef::SharedString` is the only variant that does not exist on
/// `Data`; it maps to `shared_string_value` as documented in the contract.
/// `is_1904` is the workbook's date-system flag, stamped onto every
/// datetime cell.
#[must_use]
pub fn data_ref_value(d: &DataRef<'_>, is_1904: bool) -> pb::cell_data::Value {
    match d {
        DataRef::Int(v) => pb::cell_data::Value::IntValue(*v),
        DataRef::Float(v) => pb::cell_data::Value::FloatValue(*v),
        DataRef::String(v) => pb::cell_data::Value::StringValue(v.clone()),
        DataRef::SharedString(v) => pb::cell_data::Value::SharedStringValue((*v).to_string()),
        DataRef::Bool(v) => pb::cell_data::Value::BoolValue(*v),
        DataRef::DateTime(v) => pb::cell_data::Value::DateTime(excel_date_time(v, is_1904)),
        DataRef::DateTimeIso(v) => pb::cell_data::Value::DateTimeIso(v.clone()),
        DataRef::DurationIso(v) => pb::cell_data::Value::DurationIso(v.clone()),
        DataRef::Error(e) => pb::cell_data::Value::Error(cell_error_type(e) as i32),
        DataRef::Empty => pb::cell_data::Value::Empty(()),
    }
}

/// Workbook-level 1904 date-system flag.
///
/// This is the API shape the calamine maintainer prefers (see calamine
/// PR #630): the epoch is a property of the workbook, read once via
/// `has_1904_epoch`, not of individual cells. ODS stores dates as ISO
/// strings and has no serial epoch, so it reports `false`.
#[must_use]
pub fn has_1904_epoch<RS: std::io::Read + std::io::Seek>(workbook: &calamine::Sheets<RS>) -> bool {
    match workbook {
        calamine::Sheets::Xls(xls) => xls.has_1904_epoch(),
        calamine::Sheets::Xlsx(xlsx) => xlsx.has_1904_epoch(),
        calamine::Sheets::Xlsb(xlsb) => xlsb.has_1904_epoch(),
        calamine::Sheets::Ods(_) => false,
    }
}

/// Wrap a oneof value into a `CellData` message.
#[must_use]
pub fn cell_data(value: pb::cell_data::Value) -> pb::CellData {
    pb::CellData { value: Some(value) }
}

/// Build an empty `CellData` (`Data::Empty`).
#[must_use]
pub fn empty_cell_data() -> pb::CellData {
    cell_data(pb::cell_data::Value::Empty(()))
}

/// Map a `(row, col)` tuple onto `CellPosition`.
#[must_use]
pub fn cell_position(pos: (u32, u32)) -> pb::CellPosition {
    pb::CellPosition {
        row: pos.0,
        col: pos.1,
    }
}

/// Map `calamine::Dimensions` onto `Dimensions`.
#[must_use]
pub fn dimensions(d: calamine::Dimensions) -> pb::Dimensions {
    pb::Dimensions {
        start: Some(cell_position(d.start)),
        end: Some(cell_position(d.end)),
    }
}

/// Map `calamine::SheetType` onto `SheetType`.
#[must_use]
pub fn sheet_type(t: calamine::SheetType) -> pb::SheetType {
    match t {
        calamine::SheetType::WorkSheet => pb::SheetType::Worksheet,
        calamine::SheetType::DialogSheet => pb::SheetType::DialogSheet,
        calamine::SheetType::MacroSheet => pb::SheetType::MacroSheet,
        calamine::SheetType::ChartSheet => pb::SheetType::ChartSheet,
        calamine::SheetType::Vba => pb::SheetType::Vba,
    }
}

/// Map `calamine::SheetVisible` onto `SheetVisible`.
#[must_use]
pub fn sheet_visible(v: SheetVisible) -> pb::SheetVisible {
    match v {
        SheetVisible::Visible => pb::SheetVisible::Visible,
        SheetVisible::Hidden => pb::SheetVisible::Hidden,
        SheetVisible::VeryHidden => pb::SheetVisible::VeryHidden,
    }
}

/// Map `calamine::Sheet` onto `Sheet`.
#[must_use]
pub fn sheet(s: &Sheet) -> pb::Sheet {
    pb::Sheet {
        name: s.name.clone(),
        typ: sheet_type(s.typ) as i32,
        visible: sheet_visible(s.visible) as i32,
    }
}

/// Build the `CalamineErrorKind` matching a top-level `calamine::Error`
/// variant.
#[must_use]
pub fn error_kind(e: &calamine::Error) -> pb::CalamineErrorKind {
    match e {
        calamine::Error::Io(_) => pb::CalamineErrorKind::Io,
        calamine::Error::Ods(_) => pb::CalamineErrorKind::Ods,
        calamine::Error::Xls(_) => pb::CalamineErrorKind::Xls,
        calamine::Error::Xlsb(_) => pb::CalamineErrorKind::Xlsb,
        calamine::Error::Xlsx(_) => pb::CalamineErrorKind::Xlsx,
        calamine::Error::Vba(_) => pb::CalamineErrorKind::Vba,
        calamine::Error::De(_) => pb::CalamineErrorKind::De,
        calamine::Error::Msg(_) => pb::CalamineErrorKind::Msg,
    }
}

/// Build a `CalamineError` from any displayable error plus its kind.
#[must_use]
pub fn calamine_error(
    kind: pb::CalamineErrorKind,
    err: impl std::fmt::Display,
) -> pb::CalamineError {
    pb::CalamineError {
        kind: kind as i32,
        message: err.to_string(),
    }
}

/// The `CalamineErrorKind` for errors produced by a reader of the given
/// workbook format.
#[must_use]
pub fn error_kind_for_format(format: pb::WorkbookFormat) -> pb::CalamineErrorKind {
    match format {
        pb::WorkbookFormat::Xls => pb::CalamineErrorKind::Xls,
        pb::WorkbookFormat::Xlsx => pb::CalamineErrorKind::Xlsx,
        pb::WorkbookFormat::Xlsb => pb::CalamineErrorKind::Xlsb,
        pb::WorkbookFormat::Ods => pb::CalamineErrorKind::Ods,
        pb::WorkbookFormat::Unspecified => pb::CalamineErrorKind::Unspecified,
    }
}

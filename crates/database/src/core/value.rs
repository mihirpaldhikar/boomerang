/*
 * Copyright (c) Mihir Paldhikar
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the “Software”), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED “AS IS”, WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 */

use crate::core::symbol_interner::Symbol;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum ValueType {
    Null = 0,
    Bool = 1,
    Int = 2,
    Float = 3,
    Decimal = 4,
    Text = 5,
    Blob = 6,
    Json = 7,
    Uuid = 8,
    Timestamp = 9,
    TimestampTz = 10,
    Date = 11,
    Time = 12,
    TimeTz = 13,
    Custom(Symbol) = 14,
    BoolArray = 15,
    IntArray = 16,
    FloatArray = 17,
    DecimalArray = 18,
    TextArray = 19,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomValue {
    pub name: Symbol,
    pub value: Box<str>,
}

pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Decimal(rust_decimal::Decimal),
    Text(Box<str>),
    Blob(Box<[u8]>),
    Json(Box<serde_json::Value>),
    Uuid(uuid::Uuid),
    Timestamp(chrono::NaiveDateTime),
    TimestampTz(chrono::DateTime<chrono::Utc>),
    Date(chrono::NaiveDate),
    Time(chrono::NaiveTime),
    TimeTz(chrono::DateTime<chrono::FixedOffset>),
    Custom(Box<CustomValue>),
    BoolArray(Box<[bool]>),
    IntArray(Box<[i64]>),
    FloatArray(Box<[f64]>),
    DecimalArray(Box<[rust_decimal::Decimal]>),
    TextArray(Box<[Box<str>]>),
}

impl Value {
    pub fn kind(&self) -> ValueType {
        match self {
            Value::Null => ValueType::Null,
            Value::Bool(_) => ValueType::Bool,
            Value::Int(_) => ValueType::Int,
            Value::Float(_) => ValueType::Float,
            Value::Decimal(_) => ValueType::Decimal,
            Value::Text(_) => ValueType::Text,
            Value::Blob(_) => ValueType::Blob,
            Value::Json(_) => ValueType::Json,
            Value::Uuid(_) => ValueType::Uuid,
            Value::Timestamp(_) => ValueType::Timestamp,
            Value::TimestampTz(_) => ValueType::TimestampTz,
            Value::Date(_) => ValueType::Date,
            Value::Time(_) => ValueType::Time,
            Value::TimeTz(_) => ValueType::TimeTz,
            Value::Custom(custom) => ValueType::Custom(custom.name),
            Value::BoolArray(_) => ValueType::BoolArray,
            Value::IntArray(_) => ValueType::IntArray,
            Value::FloatArray(_) => ValueType::FloatArray,
            Value::DecimalArray(_) => ValueType::DecimalArray,
            Value::TextArray(_) => ValueType::TextArray,
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }
}

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Value::Null, Value::Null) => true,
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::Int(a), Value::Int(b)) => a == b,
            (Value::Float(a), Value::Float(b)) => a.to_bits() == b.to_bits(),
            (Value::Decimal(a), Value::Decimal(b)) => a == b,
            (Value::Text(a), Value::Text(b)) => a == b,
            (Value::Blob(a), Value::Blob(b)) => a == b,
            (Value::Json(a), Value::Json(b)) => a == b,
            (Value::Uuid(a), Value::Uuid(b)) => a == b,
            (Value::Timestamp(a), Value::Timestamp(b)) => a == b,
            (Value::TimestampTz(a), Value::TimestampTz(b)) => a == b,
            (Value::Date(a), Value::Date(b)) => a == b,
            (Value::Time(a), Value::Time(b)) => a == b,
            (Value::TimeTz(a), Value::TimeTz(b)) => a == b,
            (Value::Custom(a), Value::Custom(b)) => a == b,
            (Value::BoolArray(a), Value::BoolArray(b)) => a == b,
            (Value::IntArray(a), Value::IntArray(b)) => a == b,
            (Value::FloatArray(a), Value::FloatArray(b)) => a == b,
            (Value::DecimalArray(a), Value::DecimalArray(b)) => a == b,
            (Value::TextArray(a), Value::TextArray(b)) => a == b,
            _ => false,
        }
    }
}

impl Eq for Value {}

macro_rules! impl_int {
    ($($t:ty),*) => {
        $(
            impl From<$t> for Value {
                fn from(val: $t) -> Self {
                    Value::Int(val as i64)
                }
            }
        )*
    };
}

impl_int!(i8, i16, i32, i64, u8, u16, u32);

macro_rules! impl_float {
    ($($t:ty),*) => {
        $(
            impl From<$t> for Value {
                fn from(val: $t) -> Self {
                    Value::Float(val as f64)
                }
            }
        )*
    };
}

impl_float!(f32, f64);

impl From<bool> for Value {
    fn from(value: bool) -> Self {
        Value::Bool(value)
    }
}

impl From<String> for Value {
    fn from(value: String) -> Self {
        Value::Text(value.into_boxed_str())
    }
}

impl From<Vec<u8>> for Value {
    fn from(value: Vec<u8>) -> Self {
        Value::Blob(value.into_boxed_slice())
    }
}

impl From<Box<[u8]>> for Value {
    fn from(value: Box<[u8]>) -> Self {
        Value::Blob(value)
    }
}

impl From<serde_json::Value> for Value {
    fn from(value: serde_json::Value) -> Self {
        Value::Json(Box::new(value))
    }
}

impl From<chrono::NaiveDateTime> for Value {
    fn from(value: chrono::NaiveDateTime) -> Self {
        Value::Timestamp(value)
    }
}

impl From<chrono::DateTime<chrono::Utc>> for Value {
    fn from(value: chrono::DateTime<chrono::Utc>) -> Self {
        Value::TimestampTz(value)
    }
}

impl From<uuid::Uuid> for Value {
    fn from(value: uuid::Uuid) -> Self {
        Value::Uuid(value)
    }
}

impl From<rust_decimal::Decimal> for Value {
    fn from(value: rust_decimal::Decimal) -> Self {
        Value::Decimal(value)
    }
}

impl From<Vec<i64>> for Value {
    fn from(value: Vec<i64>) -> Self {
        Value::IntArray(value.into_boxed_slice())
    }
}

impl From<Box<[i64]>> for Value {
    fn from(value: Box<[i64]>) -> Self {
        Value::IntArray(value)
    }
}

macro_rules! impl_int_array {
    ($($t:ty),*) => {
        $(
            impl From<Vec<$t>> for Value {
                fn from(value: Vec<$t>) -> Self {
                    Value::IntArray(value
                            .into_iter()
                            .map(|v| v as i64)
                            .collect::<Vec<i64>>()
                            .into_boxed_slice())
                }
            }

        impl From<Box<[$t]>> for Value {
                fn from(value: Box<[$t]>) -> Self {
                    Value::IntArray(
                        value
                            .into_vec()
                            .into_iter()
                            .map(|v| v as i64)
                            .collect::<Vec<i64>>()
                            .into_boxed_slice()
                    )
                }
            }
        )*
    };
}

impl_int_array!(i8, i16, i32, u16, u32);

impl From<Vec<f64>> for Value {
    fn from(value: Vec<f64>) -> Self {
        Value::FloatArray(value.into_boxed_slice())
    }
}

impl From<Box<[f64]>> for Value {
    fn from(value: Box<[f64]>) -> Self {
        Value::FloatArray(value)
    }
}

macro_rules! impl_float_array {
    ($($t:ty),*) => {
        $(
            impl From<Vec<$t>> for Value {
                fn from(value: Vec<$t>) -> Self {
                    Value::FloatArray(value
                            .into_iter()
                            .map(|v| v as f64)
                            .collect::<Vec<f64>>()
                            .into_boxed_slice())
                }
            }

        impl From<Box<[$t]>> for Value {
                fn from(value: Box<[$t]>) -> Self {
                    Value::FloatArray(
                        value
                            .into_vec()
                            .into_iter()
                            .map(|v| v as f64)
                            .collect::<Vec<f64>>()
                            .into_boxed_slice()
                    )
                }
            }
        )*
    };
}

impl_float_array!(f32);

impl From<Vec<bool>> for Value {
    fn from(value: Vec<bool>) -> Self {
        Value::BoolArray(value.into_boxed_slice())
    }
}

impl From<Box<[bool]>> for Value {
    fn from(value: Box<[bool]>) -> Self {
        Value::BoolArray(value)
    }
}

impl From<Vec<rust_decimal::Decimal>> for Value {
    fn from(value: Vec<rust_decimal::Decimal>) -> Self {
        Value::DecimalArray(value.into_boxed_slice())
    }
}

impl From<Box<[rust_decimal::Decimal]>> for Value {
    fn from(value: Box<[rust_decimal::Decimal]>) -> Self {
        Value::DecimalArray(value)
    }
}

impl From<Vec<String>> for Value {
    fn from(value: Vec<String>) -> Self {
        let values: Vec<Box<str>> = value.into_iter().map(String::into_boxed_str).collect();

        Value::TextArray(values.into_boxed_slice())
    }
}

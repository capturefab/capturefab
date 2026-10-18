//! Keep the field path when typed deserialization adds constraints beyond
//! the portable schema (for example a camera coordinate larger than i64).
use serde::de::{
    self, DeserializeOwned, DeserializeSeed, IntoDeserializer, MapAccess, SeqAccess, Visitor,
};
use std::{cell::RefCell, fmt, rc::Rc};

type Failure = Rc<RefCell<Option<String>>>;

pub(super) fn parse<T: DeserializeOwned>(
    value: serde_json::Value,
) -> Result<T, (Option<String>, serde_json::Error)> {
    let failed = Rc::new(RefCell::new(None));
    T::deserialize(At {
        inner: value,
        path: String::new(),
        failed: failed.clone(),
    })
    .map_err(|error| (failed.borrow().clone().filter(|s| !s.is_empty()), error))
}
struct At<D> {
    inner: D,
    path: String,
    failed: Failure,
}
struct Seen<V> {
    inner: V,
    path: String,
    failed: Failure,
}
fn record(failed: &Failure, path: &str) {
    if failed.borrow().is_none() {
        *failed.borrow_mut() = Some(path.into());
    }
}
macro_rules! deserialize {
    ($($method:ident $(($($arg:ident : $ty:ty),*))?),* $(,)?) => { $(
        fn $method<V: Visitor<'de>>(self, $($($arg: $ty,)*)? visitor: V) -> Result<V::Value, D::Error> {
            let path = self.path.clone(); let failed = self.failed.clone();
            self.inner.$method($($($arg,)*)? Seen { inner: visitor, path: self.path, failed: self.failed })
                .inspect_err(|_| record(&failed, &path))
        }
    )* };
}
impl<'de, D: de::Deserializer<'de>> de::Deserializer<'de> for At<D> {
    type Error = D::Error;
    deserialize! {
        deserialize_any, deserialize_bool, deserialize_i8, deserialize_i16, deserialize_i32,
        deserialize_i64, deserialize_i128, deserialize_u8, deserialize_u16, deserialize_u32,
        deserialize_u64, deserialize_u128, deserialize_f32, deserialize_f64, deserialize_char,
        deserialize_str, deserialize_string, deserialize_bytes, deserialize_byte_buf,
        deserialize_option, deserialize_unit, deserialize_unit_struct(name: &'static str),
        deserialize_newtype_struct(name: &'static str), deserialize_seq,
        deserialize_tuple(len: usize), deserialize_tuple_struct(name: &'static str, len: usize),
        deserialize_map, deserialize_struct(name: &'static str, fields: &'static [&'static str]),
        deserialize_enum(name: &'static str, variants: &'static [&'static str]),
        deserialize_identifier, deserialize_ignored_any,
    }
    fn is_human_readable(&self) -> bool {
        self.inner.is_human_readable()
    }
}
macro_rules! visit {
    ($($method:ident($ty:ty)),* $(,)?) => { $(
        fn $method<E: de::Error>(self, value: $ty) -> Result<V::Value, E> { self.inner.$method(value) }
    )* };
}
impl<'de, V: Visitor<'de>> Visitor<'de> for Seen<V> {
    type Value = V::Value;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.inner.expecting(f)
    }
    visit! { visit_bool(bool), visit_i8(i8), visit_i16(i16), visit_i32(i32), visit_i64(i64), visit_i128(i128), visit_u8(u8), visit_u16(u16), visit_u32(u32), visit_u64(u64), visit_u128(u128), visit_f32(f32), visit_f64(f64), visit_char(char), visit_str(&str), visit_borrowed_str(&'de str), visit_string(String), visit_bytes(&[u8]), visit_borrowed_bytes(&'de [u8]), visit_byte_buf(Vec<u8>) }
    fn visit_none<E: de::Error>(self) -> Result<V::Value, E> {
        self.inner.visit_none()
    }
    fn visit_unit<E: de::Error>(self) -> Result<V::Value, E> {
        self.inner.visit_unit()
    }
    fn visit_some<D: de::Deserializer<'de>>(self, inner: D) -> Result<V::Value, D::Error> {
        self.inner.visit_some(At {
            inner,
            path: self.path,
            failed: self.failed,
        })
    }
    fn visit_newtype_struct<D: de::Deserializer<'de>>(
        self,
        inner: D,
    ) -> Result<V::Value, D::Error> {
        self.inner.visit_newtype_struct(At {
            inner,
            path: self.path,
            failed: self.failed,
        })
    }
    fn visit_map<A: MapAccess<'de>>(self, inner: A) -> Result<V::Value, A::Error> {
        self.inner.visit_map(Fields {
            inner,
            path: self.path,
            failed: self.failed,
            key: String::new(),
        })
    }
    fn visit_seq<A: SeqAccess<'de>>(self, inner: A) -> Result<V::Value, A::Error> {
        self.inner.visit_seq(Elements {
            inner,
            path: self.path,
            failed: self.failed,
            index: 0,
        })
    }
    fn visit_enum<A: de::EnumAccess<'de>>(self, inner: A) -> Result<V::Value, A::Error> {
        self.inner.visit_enum(inner)
    }
}
struct Seed<S> {
    inner: S,
    path: String,
    failed: Failure,
}
impl<'de, S: DeserializeSeed<'de>> DeserializeSeed<'de> for Seed<S> {
    type Value = S::Value;
    fn deserialize<D: de::Deserializer<'de>>(self, inner: D) -> Result<S::Value, D::Error> {
        self.inner.deserialize(At {
            inner,
            path: self.path,
            failed: self.failed,
        })
    }
}
struct Fields<A> {
    inner: A,
    path: String,
    failed: Failure,
    key: String,
}
impl<'de, A: MapAccess<'de>> MapAccess<'de> for Fields<A> {
    type Error = A::Error;
    fn next_key_seed<S: DeserializeSeed<'de>>(
        &mut self,
        seed: S,
    ) -> Result<Option<S::Value>, A::Error> {
        let Some(key) = self.inner.next_key::<String>()? else {
            return Ok(None);
        };
        self.key = if self.path.is_empty() {
            key.clone()
        } else {
            format!("{}.{key}", self.path)
        };
        seed.deserialize(key.into_deserializer()).map(Some)
    }
    fn next_value_seed<S: DeserializeSeed<'de>>(&mut self, seed: S) -> Result<S::Value, A::Error> {
        self.inner.next_value_seed(Seed {
            inner: seed,
            path: self.key.clone(),
            failed: self.failed.clone(),
        })
    }
    fn size_hint(&self) -> Option<usize> {
        self.inner.size_hint()
    }
}
struct Elements<A> {
    inner: A,
    path: String,
    failed: Failure,
    index: usize,
}
impl<'de, A: SeqAccess<'de>> SeqAccess<'de> for Elements<A> {
    type Error = A::Error;
    fn next_element_seed<S: DeserializeSeed<'de>>(
        &mut self,
        seed: S,
    ) -> Result<Option<S::Value>, A::Error> {
        let path = format!("{}[{}]", self.path, self.index);
        self.index += 1;
        self.inner.next_element_seed(Seed {
            inner: seed,
            path,
            failed: self.failed.clone(),
        })
    }
    fn size_hint(&self) -> Option<usize> {
        self.inner.size_hint()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    #[derive(Deserialize)]
    struct Args {
        points: Vec<Point>,
    }
    #[derive(Deserialize)]
    struct Point {
        x: i64,
    }
    #[test]
    fn reports_the_nested_path_of_typed_overflows() {
        let (_, error) = parse::<Args>(serde_json::json!({"points":[{"x":0},{"x":u64::MAX}]}))
            .err()
            .unwrap();
        assert!(error.to_string().contains("i64"));
        let (field, _) = parse::<Args>(serde_json::json!({"points":[{"x":u64::MAX}]}))
            .err()
            .unwrap();
        assert_eq!(field.as_deref(), Some("points[0].x"));
        let args = parse::<Args>(serde_json::json!({"points":[{"x":10}]})).unwrap();
        assert_eq!(args.points[0].x, 10);
    }
}

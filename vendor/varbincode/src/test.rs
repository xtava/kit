use super::{deserialize, serialize, DecodeLimits};
use serde::Serialize;
use serde_derive::*;
use std::collections::HashMap;
use std::fmt;

fn same<'de, T: serde::de::DeserializeOwned + Serialize + std::fmt::Debug + PartialEq>(a: T) {
    let encoded = serialize(&a).unwrap();
    let decoded: T = deserialize(encoded.as_slice(), generous_limits()).unwrap();
    assert_eq!(decoded, a);
    eprintln!("{:?} encoded as {:?}", a, encoded);
}

fn generous_limits() -> DecodeLimits {
    DecodeLimits {
        max_owned_payload_bytes: 1 << 20,
        max_string_bytes: 1 << 20,
        max_byte_buffer_bytes: 1 << 20,
        max_shape_nodes: 1 << 20,
        max_nesting_depth: 128,
    }
}

#[test]
fn test() {
    same(0u8);
    same(1u8);
    same(1i8);
    same(0i8);
    same(0u16);
    same(255u16);
    same(0xffffu16);
    same(0x7fffi16);
    same(-0x7fffi16);
    same(0x00ff_ffffu32);
    same(0xffff_ffffu32);
    same(0x00ff_ffffu64);
    same(0xffff_ffffu64);
    same(0xffff_ffff_ffffu64);
    same(0xffff_ffff_ffff_ffffu64);
    same(0f32);
    same(10.5f32);
    same(10.5f64);
    same(-10.5f64);

    same("".to_string());
    same("hello".to_string());

    same((1u8,));
    same((1u8, 2, 3));
    same((1u8, "foo".to_string()));

    same(true);
    same(false);

    same(Some(true));
    same(None::<bool>);

    same('c');
    same(b'c');
}

#[test]
fn test_structs() {
    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct Struct {
        a: isize,
        b: String,
        c: bool,
    }

    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct Outer {
        inner: Struct,
        b: bool,
        second: Struct,
    }

    same(Struct { a: -42, b: "hello".to_string(), c: true });

    same(Outer {
        inner: Struct { a: 1, b: "bee".to_string(), c: false },
        b: true,
        second: Struct { a: 2, b: "other".to_string(), c: true },
    });

    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct NewType(usize);
    same(NewType(123));

    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct NewTypeTuple(usize, bool);
    same(NewTypeTuple(123, true));
}

#[test]
fn test_enum() {
    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    enum TestEnum {
        NoArg,
        OneArg(usize),
        Args(usize, usize),
        AnotherNoArg,
        StructLike { x: usize, y: f32 },
    }
    same(TestEnum::NoArg);
    same(TestEnum::OneArg(4));
    same(TestEnum::Args(4, 5));
    same(TestEnum::AnotherNoArg);
    same(TestEnum::StructLike { x: 4, y: 3.14159 });
    same(vec![
        TestEnum::NoArg,
        TestEnum::OneArg(5),
        TestEnum::AnotherNoArg,
        TestEnum::StructLike { x: 4, y: 1.4 },
    ]);
}

#[test]
fn test_vec() {
    let v: Vec<u8> = vec![];
    same(v);
    same(vec![1u64]);
    same(vec![1u64, 2, 3, 4, 5, 6]);
}

#[test]
fn test_map() {
    let mut m = HashMap::new();
    m.insert(4u64, "foo".to_string());
    m.insert(0u64, "bar".to_string());
    same(m);
}

#[test]
fn test_fixed_size_array() {
    same([24u32; 32]);
    same([1u64, 2, 3, 4, 5, 6, 7, 8]);
    same([0u8; 19]);
}

#[test]
fn rejects_payload_before_allocation() {
    let encoded = serialize(&"abc".to_string()).unwrap();
    let mut limits = generous_limits();
    limits.max_owned_payload_bytes = 2;
    let error = deserialize::<String, _>(encoded.as_slice(), limits).unwrap_err();
    assert_eq!(error, super::error::Error::LimitExceeded { kind: "owned payload bytes", limit: 2 });
}

#[test]
fn rejects_declared_sequence_and_map_lengths() {
    let encoded = serialize(&vec![1u64, 2, 3]).unwrap();
    let mut limits = generous_limits();
    limits.max_shape_nodes = 3;
    let error = deserialize::<Vec<u64>, _>(encoded.as_slice(), limits).unwrap_err();
    assert_eq!(error, super::error::Error::LimitExceeded { kind: "shape nodes", limit: 3 });

    let mut map = HashMap::new();
    map.insert(1u64, 1u64);
    map.insert(2u64, 2u64);
    let encoded = serialize(&map).unwrap();
    let mut limits = generous_limits();
    limits.max_shape_nodes = 1;
    let error = deserialize::<HashMap<u64, u64>, _>(encoded.as_slice(), limits).unwrap_err();
    assert_eq!(error, super::error::Error::LimitExceeded { kind: "shape nodes", limit: 1 });
}

#[test]
fn shape_nodes_accumulate_across_sibling_containers() {
    let encoded = serialize(&vec![vec![1u64], vec![2u64]]).unwrap();

    // outer seq: 2 declared items + 1 container; each inner seq: 1 declared item + 1 container.
    let mut limits = generous_limits();
    limits.max_shape_nodes = 7;
    assert_eq!(
        deserialize::<Vec<Vec<u64>>, _>(encoded.as_slice(), limits).unwrap(),
        vec![vec![1u64], vec![2u64]]
    );

    let mut limits = generous_limits();
    limits.max_shape_nodes = 6;
    let error = deserialize::<Vec<Vec<u64>>, _>(encoded.as_slice(), limits).unwrap_err();
    assert_eq!(error, super::error::Error::LimitExceeded { kind: "shape nodes", limit: 6 });
}

#[test]
fn rejects_nesting_beyond_the_permitted_depth() {
    let encoded = serialize(&vec![vec![1u64], vec![2u64]]).unwrap();
    let mut limits = generous_limits();
    limits.max_nesting_depth = 1;
    let error = deserialize::<Vec<Vec<u64>>, _>(encoded.as_slice(), limits).unwrap_err();
    assert_eq!(error, super::error::Error::LimitExceeded { kind: "nesting depth", limit: 1 });
}

#[derive(Debug, PartialEq)]
struct NoHintSequence(Vec<u64>);

impl<'de> serde::Deserialize<'de> for NoHintSequence {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct Visitor;

        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = NoHintSequence;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a sequence without an allocation hint")
            }

            fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::SeqAccess<'de>,
            {
                assert_eq!(sequence.size_hint(), None);
                let mut values = Vec::new();
                while let Some(value) = sequence.next_element()? {
                    values.push(value);
                }
                Ok(NoHintSequence(values))
            }
        }

        deserializer.deserialize_seq(Visitor)
    }
}

#[derive(Debug, PartialEq)]
struct NoHintMap(HashMap<u64, u64>);

impl<'de> serde::Deserialize<'de> for NoHintMap {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct Visitor;

        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = NoHintMap;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a map without an allocation hint")
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                assert_eq!(map.size_hint(), None);
                let mut values = HashMap::new();
                while let Some((key, value)) = map.next_entry()? {
                    values.insert(key, value);
                }
                Ok(NoHintMap(values))
            }
        }

        deserializer.deserialize_map(Visitor)
    }
}

#[test]
fn hides_attacker_controlled_collection_capacity_hints() {
    let encoded = serialize(&vec![1u64, 2, 3]).unwrap();
    assert_eq!(
        deserialize::<NoHintSequence, _>(encoded.as_slice(), generous_limits()).unwrap(),
        NoHintSequence(vec![1, 2, 3])
    );

    let mut values = HashMap::new();
    values.insert(1u64, 2u64);
    let encoded = serialize(&values).unwrap();
    assert_eq!(
        deserialize::<NoHintMap, _>(encoded.as_slice(), generous_limits()).unwrap(),
        NoHintMap(values)
    );
}

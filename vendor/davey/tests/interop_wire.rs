use std::io::{Read, Write};

pub const FRAME: [u8; 6] = [0xf8, 0x01, 0x02, 0x03, 0x04, 0x05];

pub fn read_frame(input: &mut impl Read) -> Vec<u8> {
  let mut length = [0; 4];
  input.read_exact(&mut length).unwrap();
  let length = u32::from_be_bytes(length) as usize;
  assert!(
    length <= 65_536,
    "interoperability frame exceeds test limit"
  );
  let mut bytes = vec![0; length];
  input.read_exact(&mut bytes).unwrap();
  bytes
}

pub fn write_frame(output: &mut impl Write, bytes: &[u8]) {
  assert!(bytes.len() <= 65_536);
  output
    .write_all(&(bytes.len() as u32).to_be_bytes())
    .unwrap();
  output.write_all(bytes).unwrap();
  output.flush().unwrap();
}

#[test]
#[should_panic(expected = "interoperability frame exceeds test limit")]
fn rejects_oversized_frame_before_allocating() {
  read_frame(&mut u32::MAX.to_be_bytes().as_slice());
}

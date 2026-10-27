use davey::{DaveSession, MediaType};
use std::{io, num::NonZeroU16};

#[path = "../../interop_wire.rs"]
mod wire;

fn main() {
  let mut input = io::stdin().lock();
  let mut output = io::stdout().lock();
  let mut bob = DaveSession::new(NonZeroU16::new(1).unwrap(), 222, 999, None).unwrap();
  bob
    .set_external_sender(&wire::read_frame(&mut input))
    .unwrap();
  wire::write_frame(&mut output, &bob.create_key_package().unwrap());
  bob.process_welcome(&wire::read_frame(&mut input)).unwrap();
  wire::write_frame(
    &mut output,
    bob.get_epoch_authenticator().unwrap().as_slice(),
  );
  assert_eq!(
    bob
      .decrypt(111, MediaType::AUDIO, &wire::read_frame(&mut input))
      .unwrap(),
    wire::FRAME
  );
  wire::write_frame(&mut output, &bob.encrypt_opus(&wire::FRAME).unwrap());
}

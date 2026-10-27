use super::*;

#[path = "interop_wire.rs"]
mod wire;

fn session(user_id: u64) -> DaveSession {
  DaveSession::new(NonZeroU16::new(1).unwrap(), user_id, 999, None).unwrap()
}

fn external_sender() -> Vec<u8> {
  let signer = SignatureKeyPair::new(
    Ciphersuite::MLS_128_DHKEMP256_AES128GCM_SHA256_P256.signature_algorithm(),
  )
  .unwrap();
  ExternalSender::new(
    signer.public().to_vec().into(),
    BasicCredential::new(b"synthetic delivery service".to_vec()).into(),
  )
  .tls_serialize_detached()
  .unwrap()
}

fn add_member(alice: &mut DaveSession, key_package: &[u8]) -> (Vec<u8>, Vec<u8>) {
  let key_package = KeyPackageIn::tls_deserialize_exact_bytes(key_package)
    .unwrap()
    .validate(alice.provider.crypto(), ProtocolVersion::Mls10)
    .unwrap();
  let (commit, welcome, _) = alice
    .group
    .as_mut()
    .unwrap()
    .add_members(&alice.provider, &alice.signer, &[key_package])
    .unwrap();
  alice.status = SessionStatus::AWAITING_RESPONSE;
  let openmls::framing::MlsMessageBodyOut::Welcome(welcome) = welcome.body() else {
    panic!("adding a member did not produce a Welcome");
  };
  (
    commit.tls_serialize_detached().unwrap(),
    welcome.tls_serialize_detached().unwrap(),
  )
}

#[test]
fn own_commit_welcome_and_encrypted_audio_round_trip() {
  let mut alice = session(111);
  let mut bob = session(222);
  let sender = external_sender();
  alice.set_external_sender(&sender).unwrap();
  bob.set_external_sender(&sender).unwrap();
  let (commit, welcome) = add_member(&mut alice, &bob.create_key_package().unwrap());

  let mut invalid_commit = commit.clone();
  *invalid_commit.last_mut().unwrap() ^= 1;
  let pending_epoch = alice.epoch();
  assert!(alice.process_commit(&invalid_commit).is_err());
  assert_eq!(alice.epoch(), pending_epoch);
  alice.process_commit(&commit).unwrap();
  assert!(alice.process_commit(&commit).is_err());
  bob.process_welcome(&welcome).unwrap();
  assert_eq!(alice.status(), SessionStatus::ACTIVE);
  assert_eq!(alice.epoch(), bob.epoch());
  assert_eq!(
    alice.get_epoch_authenticator().unwrap().as_slice(),
    bob.get_epoch_authenticator().unwrap().as_slice()
  );

  let frame = [0xf8, 0x01, 0x02, 0x03, 0x04, 0x05];
  assert_ne!(frame.as_slice(), OPUS_SILENCE_PACKET);
  let encrypted = alice.encrypt_opus(&frame).unwrap().into_owned();
  assert_ne!(encrypted, frame);
  assert_eq!(
    bob.decrypt(111, MediaType::AUDIO, &encrypted).unwrap(),
    frame
  );

  let mut tampered = alice.encrypt_opus(&frame).unwrap().into_owned();
  tampered[1] ^= 1;
  assert!(bob.decrypt(111, MediaType::AUDIO, &tampered).is_err());

  let encrypted = bob.encrypt_opus(&frame).unwrap().into_owned();
  assert_eq!(
    alice.decrypt(222, MediaType::AUDIO, &encrypted).unwrap(),
    frame
  );
}

#[test]
fn welcome_with_a_different_external_sender_is_rejected() {
  let mut alice = session(111);
  let mut bob = session(222);
  alice.set_external_sender(&external_sender()).unwrap();
  bob.set_external_sender(&external_sender()).unwrap();
  let (_, welcome) = add_member(&mut alice, &bob.create_key_package().unwrap());
  assert!(matches!(
    bob.process_welcome(&welcome),
    Err(ProcessWelcomeError::UnexpectedExternalSender)
  ));
  assert!(!bob.is_ready());
}

#[test]
fn welcome_and_encrypted_frames_interoperate_with_published_davey() {
  use std::process::{Child, Command, Stdio};

  struct Peer(Child);
  impl Drop for Peer {
    fn drop(&mut self) {
      if matches!(self.0.try_wait(), Ok(None)) {
        let _ = self.0.kill();
        let _ = self.0.wait();
      }
    }
  }

  let executable = std::env::var("VOZEN_LEGACY_DAVE_PEER")
    .expect("build the published Davey peer and set VOZEN_LEGACY_DAVE_PEER");
  let mut peer = Peer(
    Command::new(executable)
      .stdin(Stdio::piped())
      .stdout(Stdio::piped())
      .spawn()
      .unwrap(),
  );
  let mut input = peer.0.stdout.take().unwrap();
  let mut output = peer.0.stdin.take().unwrap();
  let sender = external_sender();
  let mut alice = session(111);
  alice.set_external_sender(&sender).unwrap();
  wire::write_frame(&mut output, &sender);
  let (commit, welcome) = add_member(&mut alice, &wire::read_frame(&mut input));
  alice.process_commit(&commit).unwrap();
  wire::write_frame(&mut output, &welcome);
  assert_eq!(
    wire::read_frame(&mut input),
    alice.get_epoch_authenticator().unwrap().as_slice()
  );
  wire::write_frame(&mut output, &alice.encrypt_opus(&wire::FRAME).unwrap());
  assert_eq!(
    alice
      .decrypt(222, MediaType::AUDIO, &wire::read_frame(&mut input))
      .unwrap(),
    wire::FRAME
  );
  drop(output);
  assert!(peer.0.wait().unwrap().success());
}

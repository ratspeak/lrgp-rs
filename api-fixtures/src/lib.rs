//! External-consumer compile contract for canonical and retained LRGP paths.

pub mod canonical {
    use lrgp::protocol::{
        CMD_CHALLENGE, ENVELOPE_MAX_PACKED, Envelope, LrgpError, PROTOCOL_TYPE,
        ValidatedEnvelope, pack_envelope, pack_into_preencoded_fields, validate_envelope,
    };

    pub fn compile_surface(envelope: &Envelope) -> Result<ValidatedEnvelope, LrgpError> {
        let _ = (CMD_CHALLENGE, ENVELOPE_MAX_PACKED, PROTOCOL_TYPE);
        let packed = pack_envelope("ttt", 1, "challenge", "0123456789abcdef", None, None)?;
        let _ = pack_into_preencoded_fields(&packed)?;
        validate_envelope(envelope)
    }
}

pub mod retained {
    use lrgp::constants::PROTOCOL_TYPE;
    use lrgp::envelope::{Envelope, validate_envelope};
    use lrgp::errors::LrgpError;
    use lrgp::transport::pack_into_preencoded_fields;

    pub fn compile_surface(envelope: &Envelope) -> Result<(), LrgpError> {
        let _ = PROTOCOL_TYPE;
        let _ = validate_envelope(envelope)?;
        let _ = pack_into_preencoded_fields(envelope)?;
        Ok(())
    }
}

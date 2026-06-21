pub fn event_to_narrative(event: &crate::expert_surgery::SurgeryEvent) -> String {
    match event {
        crate::expert_surgery::SurgeryEvent::ProModelInvoked => {
            "\n[agent:expert] Pro model invoked for code surgery...\n".to_string()
        }
        crate::expert_surgery::SurgeryEvent::ProModelGraphDone { elapsed_ms } => {
            format!("\n[agent:expert] Pro model execution completed in {}ms.\n", elapsed_ms)
        }
        crate::expert_surgery::SurgeryEvent::PreWriteVerificationStarted => {
            "\n[agent:verify] Pre-write consistency verification started...\n".to_string()
        }
        crate::expert_surgery::SurgeryEvent::FileConsistentApproved => {
            "\n[agent:verify] Disk file is consistent with pre-await snapshot. Approved for writing.\n".to_string()
        }
        crate::expert_surgery::SurgeryEvent::OffsetDriftDetected => {
            "\n[agent:drift] Disk change/offset drift detected. Initializing 3-way relocation...\n".to_string()
        }
        crate::expert_surgery::SurgeryEvent::ThreeWayRelocationSuccess { byte_range } => {
            format!("\n[agent:drift] 3-way relocation succeeded. Offset adjusted to range {}..{}.\n", byte_range.0, byte_range.1)
        }
        crate::expert_surgery::SurgeryEvent::HardConflictEncountered => {
            "\n[agent:drift] Hard conflict encountered: could not align patch block on disk.\n".to_string()
        }
        crate::expert_surgery::SurgeryEvent::FlashResolverStarted => {
            "\n[agent:expert] Flash model resolver started for semantic merge...\n".to_string()
        }
        crate::expert_surgery::SurgeryEvent::FlashResolverSuccess => {
            "\n[agent:expert] Flash resolver successfully merged conflict blocks.\n".to_string()
        }
        crate::expert_surgery::SurgeryEvent::LocalLintStarted => {
            "\n[agent:verify] Local verification (AST check & cargo check) started...\n".to_string()
        }
        crate::expert_surgery::SurgeryEvent::SyntaxTreeVerified => {
            "\n[agent:verify] AST syntax tree verified successfully.\n".to_string()
        }
        crate::expert_surgery::SurgeryEvent::CargoCheckPassed => {
            "\n[agent:verify] Local compilation check passed.\n".to_string()
        }
        crate::expert_surgery::SurgeryEvent::TransactionRolledBack { reason } => {
            format!("\n[agent:verify] Transaction rolled back: {}. Restoring original file contents.\n", reason)
        }
    }
}


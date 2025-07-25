use dicom_core::{dicom_value, DataElement, VR};
use dicom_dictionary_std::{tags, uids::*};
use dicom_encoding::transfer_syntax::TransferSyntaxIndex;
use dicom_object::{FileMetaTableBuilder, InMemDicomObject, StandardDataDictionary};
use dicom_transfer_syntax_registry::TransferSyntaxRegistry;
use dicom_ul::{pdu::PDataValueType, Pdu};
use snafu::{OptionExt, Report, ResultExt, Whatever};
use tracing::{debug, info, warn};

use crate::App;

/// A list of supported abstract syntaxes for storage services
#[allow(deprecated)]
pub static ABSTRACT_SYNTAXES: &[&str] = &[
    CT_IMAGE_STORAGE,
    ENHANCED_CT_IMAGE_STORAGE,
    STANDALONE_CURVE_STORAGE,
    STANDALONE_OVERLAY_STORAGE,
    SECONDARY_CAPTURE_IMAGE_STORAGE,
    ULTRASOUND_IMAGE_STORAGE_RETIRED,
    NUCLEAR_MEDICINE_IMAGE_STORAGE_RETIRED,
    MR_IMAGE_STORAGE,
    ENHANCED_MR_IMAGE_STORAGE,
    MR_SPECTROSCOPY_STORAGE,
    ENHANCED_MR_COLOR_IMAGE_STORAGE,
    ULTRASOUND_MULTI_FRAME_IMAGE_STORAGE_RETIRED,
    COMPUTED_RADIOGRAPHY_IMAGE_STORAGE,
    DIGITAL_X_RAY_IMAGE_STORAGE_FOR_PRESENTATION,
    DIGITAL_X_RAY_IMAGE_STORAGE_FOR_PROCESSING,
    ENCAPSULATED_PDF_STORAGE,
    ENCAPSULATED_CDA_STORAGE,
    ENCAPSULATED_STL_STORAGE,
    GRAYSCALE_SOFTCOPY_PRESENTATION_STATE_STORAGE,
    POSITRON_EMISSION_TOMOGRAPHY_IMAGE_STORAGE,
    BREAST_TOMOSYNTHESIS_IMAGE_STORAGE,
    BREAST_PROJECTION_X_RAY_IMAGE_STORAGE_FOR_PRESENTATION,
    BREAST_PROJECTION_X_RAY_IMAGE_STORAGE_FOR_PROCESSING,
    ENHANCED_PET_IMAGE_STORAGE,
    RT_IMAGE_STORAGE,
    NUCLEAR_MEDICINE_IMAGE_STORAGE,
    ULTRASOUND_MULTI_FRAME_IMAGE_STORAGE,
    MULTI_FRAME_SINGLE_BIT_SECONDARY_CAPTURE_IMAGE_STORAGE,
    MULTI_FRAME_GRAYSCALE_BYTE_SECONDARY_CAPTURE_IMAGE_STORAGE,
    MULTI_FRAME_GRAYSCALE_WORD_SECONDARY_CAPTURE_IMAGE_STORAGE,
    MULTI_FRAME_TRUE_COLOR_SECONDARY_CAPTURE_IMAGE_STORAGE,
    BASIC_TEXT_SR_STORAGE,
    ENHANCED_SR_STORAGE,
    COMPREHENSIVE_SR_STORAGE,
    VERIFICATION,
];

fn create_cstore_response(
    message_id: u16,
    sop_class_uid: &str,
    sop_instance_uid: &str,
) -> InMemDicomObject<StandardDataDictionary> {
    InMemDicomObject::command_from_element_iter([
        DataElement::new(
            tags::AFFECTED_SOP_CLASS_UID,
            VR::UI,
            dicom_value!(Str, sop_class_uid),
        ),
        DataElement::new(tags::COMMAND_FIELD, VR::US, dicom_value!(U16, [0x8001])),
        DataElement::new(
            tags::MESSAGE_ID_BEING_RESPONDED_TO,
            VR::US,
            dicom_value!(U16, [message_id]),
        ),
        DataElement::new(
            tags::COMMAND_DATA_SET_TYPE,
            VR::US,
            dicom_value!(U16, [0x0101]),
        ),
        DataElement::new(tags::STATUS, VR::US, dicom_value!(U16, [0x0000])),
        DataElement::new(
            tags::AFFECTED_SOP_INSTANCE_UID,
            VR::UI,
            dicom_value!(Str, sop_instance_uid),
        ),
    ])
}

pub async fn run_store_async(
    scu_stream: tokio::net::TcpStream,
    args: &App,
) -> Result<bool, Whatever> {
    let App {
        verbose,
        calling_ae_title,
        strict,
        uncompressed_only,
        promiscuous,
        max_pdu_length,
        out_dir,
        port: _,
        addr: _,
        file: _,
        query_file: _,
        query: _,
        called_ae_title: _,
        patient: _,
        study: _,
        move_destination: _,
    } = args;
    let verbose = *verbose;

    let mut instance_buffer: Vec<u8> = Vec::with_capacity(1024 * 1024);
    let mut msgid = 1;
    let mut sop_class_uid = "".to_string();
    let mut sop_instance_uid = "".to_string();

    let mut options = dicom_ul::association::ServerAssociationOptions::new()
        .accept_any()
        .ae_title(calling_ae_title)
        .strict(*strict)
        .max_pdu_length(*max_pdu_length)
        .promiscuous(*promiscuous);

    if *uncompressed_only {
        options = options
            .with_transfer_syntax("1.2.840.10008.1.2")
            .with_transfer_syntax("1.2.840.10008.1.2.1");
    } else {
        for ts in TransferSyntaxRegistry.iter() {
            if !ts.is_unsupported() {
                options = options.with_transfer_syntax(ts.uid());
            }
        }
    };

    for uid in ABSTRACT_SYNTAXES {
        options = options.with_abstract_syntax(*uid);
    }

    let mut association = options
        .establish_async(scu_stream)
        .await
        .whatever_context("could not establish association")?;

    info!("New association from {}", association.client_ae_title());
    debug!(
        "> Presentation contexts: {:?}",
        association.presentation_contexts()
    );

    loop {
        match association.receive().await {
            Ok(mut pdu) => {
                if verbose {
                    debug!("scu ----> scp: {}", pdu.short_description());
                }
                match pdu {
                    Pdu::PData { ref mut data } => {
                        if data.is_empty() {
                            debug!("Ignoring empty PData PDU");
                            continue;
                        }

                        for data_value in data {
                            if data_value.value_type == PDataValueType::Data && !data_value.is_last
                            {
                                instance_buffer.append(&mut data_value.data);
                            } else if data_value.value_type == PDataValueType::Command
                                && data_value.is_last
                            {
                                // commands are always in implicit VR LE
                                let ts =
                                    dicom_transfer_syntax_registry::entries::IMPLICIT_VR_LITTLE_ENDIAN
                                        .erased();
                                let data_value = &data_value;
                                let v = &data_value.data;

                                let obj = InMemDicomObject::read_dataset_with_ts(v.as_slice(), &ts)
                                    .whatever_context("failed to read incoming DICOM command")?;
                                obj.element(tags::COMMAND_FIELD)
                                    .whatever_context("Missing Command Field")?
                                    .uint16()
                                    .whatever_context("Command Field is not an integer")?;

                                msgid = obj
                                    .element(tags::MESSAGE_ID)
                                    .whatever_context("Missing Message ID")?
                                    .to_int()
                                    .whatever_context("Message ID is not an integer")?;
                                sop_class_uid = obj
                                    .element(tags::AFFECTED_SOP_CLASS_UID)
                                    .whatever_context("missing Affected SOP Class UID")?
                                    .to_str()
                                    .whatever_context("could not retrieve Affected SOP Class UID")?
                                    .to_string();
                                sop_instance_uid = obj
                                    .element(tags::AFFECTED_SOP_INSTANCE_UID)
                                    .whatever_context("missing Affected SOP Instance UID")?
                                    .to_str()
                                    .whatever_context(
                                        "could not retrieve Affected SOP Instance UID",
                                    )?
                                    .to_string();
                                instance_buffer.clear();
                            } else if data_value.value_type == PDataValueType::Data
                                && data_value.is_last
                            {
                                instance_buffer.append(&mut data_value.data);

                                let presentation_context = association
                                    .presentation_contexts()
                                    .iter()
                                    .find(|pc| pc.id == data_value.presentation_context_id)
                                    .whatever_context("missing presentation context")?;
                                let ts = &presentation_context.transfer_syntax;

                                let obj = InMemDicomObject::read_dataset_with_ts(
                                    instance_buffer.as_slice(),
                                    TransferSyntaxRegistry.get(ts).unwrap(),
                                )
                                .whatever_context("failed to read DICOM data object")?;
                                let file_meta = FileMetaTableBuilder::new()
                                    .media_storage_sop_class_uid(
                                        obj.element(tags::SOP_CLASS_UID)
                                            .whatever_context("missing SOP Class UID")?
                                            .to_str()
                                            .whatever_context("could not retrieve SOP Class UID")?,
                                    )
                                    .media_storage_sop_instance_uid(
                                        obj.element(tags::SOP_INSTANCE_UID)
                                            .whatever_context("missing SOP Instance UID")?
                                            .to_str()
                                            .whatever_context("missing SOP Instance UID")?,
                                    )
                                    .transfer_syntax(ts)
                                    .build()
                                    .whatever_context(
                                        "failed to build DICOM meta file information",
                                    )?;
                                let file_obj = obj.with_exact_meta(file_meta);

                                // write the files to the current directory with their SOPInstanceUID as filenames
                                let mut file_path = out_dir.clone();
                                file_path.push(
                                    sop_instance_uid.trim_end_matches('\0').to_string() + ".dcm",
                                );
                                file_obj
                                    .write_to_file(&file_path)
                                    .whatever_context("could not save DICOM object to file")?;
                                info!("Stored {}", file_path.display());

                                // send C-STORE-RSP object
                                // commands are always in implicit VR LE
                                let ts =
                                    dicom_transfer_syntax_registry::entries::IMPLICIT_VR_LITTLE_ENDIAN
                                        .erased();

                                let obj = create_cstore_response(
                                    msgid,
                                    &sop_class_uid,
                                    &sop_instance_uid,
                                );

                                let mut obj_data = Vec::new();

                                obj.write_dataset_with_ts(&mut obj_data, &ts)
                                    .whatever_context("could not write response object")?;

                                let pdu_response = Pdu::PData {
                                    data: vec![dicom_ul::pdu::PDataValue {
                                        presentation_context_id: data_value.presentation_context_id,
                                        value_type: PDataValueType::Command,
                                        is_last: true,
                                        data: obj_data,
                                    }],
                                };
                                association
                                    .send(&pdu_response)
                                    .await
                                    .whatever_context("failed to send response object to SCU")?;
                            }
                        }
                    }
                    Pdu::ReleaseRQ => {
                        association.send(&Pdu::ReleaseRP).await.unwrap_or_else(|e| {
                            warn!(
                                "Failed to send association release message to SCU: {}",
                                snafu::Report::from_error(e)
                            );
                        });
                        info!(
                            "Released association with {}",
                            association.client_ae_title()
                        );
                        break;
                    }
                    Pdu::AbortRQ { source } => {
                        warn!("Aborted connection from: {:?}", source);
                        break;
                    }
                    _ => {}
                }
            }
            Err(err @ dicom_ul::association::server::Error::Receive { .. }) => {
                if verbose {
                    info!("{}", Report::from_error(err));
                } else {
                    info!("{}", err);
                }
                break;
            }
            Err(err) => {
                warn!("Unexpected error: {}", Report::from_error(err));
                break;
            }
        }
    }

    if let Ok(peer_addr) = association.inner_stream().peer_addr() {
        info!(
            "Dropping connection with {} ({})",
            association.client_ae_title(),
            peer_addr
        );
    } else {
        info!("Dropping connection with {}", association.client_ae_title());
    }

    Ok(true)
}

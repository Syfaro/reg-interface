use std::str::FromStr;

use async_trait::async_trait;
use eyre::OptionExt;
use serde::Serialize;
use time::{Date, Duration};

use super::{DecodedData, Decoder, DecoderOutcome, DecoderType};

pub struct CacDecoder;

#[async_trait]
impl Decoder for CacDecoder {
    fn decoder_type(&self) -> DecoderType {
        DecoderType::Cac
    }

    async fn decode(&self, data: &str) -> eyre::Result<DecoderOutcome> {
        if let Ok(data) = CacData::from_str(data) {
            Ok(DecoderOutcome::DecodedData(DecodedData::Cac(data)))
        } else {
            Ok(DecoderOutcome::Skipped)
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct CacData {
    pub bar_code_version_code: char,
    pub person_designator_identifier: u32,
    pub person_designator_type_code: char,
    pub dod_edi_person_identifier: u32,
    pub person_first_name: String,
    pub person_middle_initial: Option<char>,
    pub person_surname: String,
    pub date_of_birth: Date,
    pub personnel_category_code: char,
    pub branch_code: char,
    pub personnel_entitlement_condition_type: String,
    pub rank: String,
    pub pay_plan_code: String,
    pub pay_plan_grade_code: String,
    pub card_issue_date: Date,
    pub card_expiration_date: Date,
    pub card_instance_identifier: char,
}

fn read_number(chars: &mut std::str::Chars<'_>, bytes: usize) -> eyre::Result<u32> {
    let s: String = chars.take(bytes).collect();
    eyre::ensure!(s.len() == bytes, "could not collect enough bytes");

    Ok(u32::from_str_radix(&s, 32)?)
}

fn read_string(chars: &mut std::str::Chars<'_>, bytes: usize) -> eyre::Result<String> {
    let s: String = chars.take(bytes).collect();
    eyre::ensure!(s.len() == bytes, "could not collect enough bytes");

    Ok(s.trim().to_string())
}

fn read_date(chars: &mut std::str::Chars<'_>) -> eyre::Result<Date> {
    let days = read_number(chars, 4)?;

    let date_epoch = Date::from_ordinal_date(1000, 1).unwrap();
    let date = date_epoch
        .checked_add(Duration::days(i64::from(days)))
        .ok_or_eyre("days too large")?;

    Ok(date)
}

impl FromStr for CacData {
    type Err = eyre::Report;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut chars = s.chars();

        // Because we're checking the length here, we know we won't have panics
        // unwrapping the chars iterator when we need more data.
        eyre::ensure!(
            [88, 89].contains(&s.len()),
            "data must be 88 or 89 characters, was {} characters",
            s.len()
        );

        let bar_code_version_code = chars.next().unwrap();
        eyre::ensure!(
            ['1', 'N'].contains(&bar_code_version_code),
            "unknown card format: {bar_code_version_code}"
        );

        let person_designator_identifier = read_number(&mut chars, 6)?;
        let person_designator_type_code = chars.next().unwrap();
        let dod_edi_person_identifier = read_number(&mut chars, 7)?;
        let person_first_name = read_string(&mut chars, 20)?;
        let person_surname = read_string(&mut chars, 26)?;
        let date_of_birth = read_date(&mut chars)?;
        let personnel_category_code = chars.next().unwrap();
        let branch_code = chars.next().unwrap();
        let personnel_entitlement_condition_type = read_string(&mut chars, 2)?;
        let rank = read_string(&mut chars, 6)?;
        let pay_plan_code = read_string(&mut chars, 2)?;
        let pay_plan_grade_code = read_string(&mut chars, 2)?;
        let card_issue_date = read_date(&mut chars)?;
        let card_expiration_date = read_date(&mut chars)?;
        let card_instance_identifier = chars.next().unwrap();

        let person_middle_initial = (bar_code_version_code == 'N').then(|| chars.next().unwrap());

        Ok(Self {
            bar_code_version_code,
            person_designator_identifier,
            person_designator_type_code,
            dod_edi_person_identifier,
            person_first_name,
            person_middle_initial,
            person_surname,
            date_of_birth,
            personnel_category_code,
            branch_code,
            personnel_entitlement_condition_type,
            rank,
            pay_plan_code,
            pay_plan_grade_code,
            card_issue_date,
            card_expiration_date,
            card_instance_identifier,
        })
    }
}

use crate::{
    bus::{self, LinkConfig},
    decode::{self, Value},
    map::{EMBEDDED_MAP, load_from_str},
};
use anyhow::{anyhow, bail};

pub const EXPECTED_SN: &str = "2605096069";

pub async fn run(cfg: &LinkConfig) -> anyhow::Result<()> {
    let mut ctx = bus::connect(cfg)?;
    let map = load_from_str(EMBEDDED_MAP)?;

    let serial_register = map
        .sensors
        .iter()
        .find(|sensor| sensor.name == "serial_number")
        .ok_or_else(|| anyhow!("Could not find serial number in map"))?;

    let span = bus::read_span(
        &mut ctx,
        serial_register.address,
        serial_register.encoding.word_count() as u16,
    )
    .await?;

    let serial_number = decode::decode(serial_register, &span)?;

    if let Value::Text(text) = serial_number
        && EXPECTED_SN == text
    {
        println!("Serial number matches!");
    } else {
        bail!("Serial number is wrong");
    }

    let run_state = map
        .sensors
        .iter()
        .find(|sensor| sensor.name == "run_state")
        .ok_or_else(|| anyhow!("Could not find run_state in map"))?;

    let span = bus::read_span(
        &mut ctx,
        run_state.address,
        run_state.encoding.word_count() as u16,
    )
    .await?;

    let run_state = decode::decode(run_state, &span)?;
    println!("Run state: {:?}", run_state);

    let rated_power = map
        .sensors
        .iter()
        .find(|sensor| sensor.name == "rated_power")
        .ok_or_else(|| anyhow!("Could not find rated_power in map"))?;

    let span = bus::read_span(
        &mut ctx,
        rated_power.address,
        rated_power.encoding.word_count() as u16,
    )
    .await?;

    let rated_power = decode::decode(rated_power, &span)?;
    println!("Rated power: {:?}", rated_power);

    Ok(())
}

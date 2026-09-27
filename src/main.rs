use solar_controller::map;

fn main() -> anyhow::Result<()> {
    let map = map::load_from_str(map::EMBEDDED_MAP)?;

    println!(
        "map ok: {} sensors, {} controls",
        map.sensors.len(),
        map.controls.len()
    );

    Ok(())
}

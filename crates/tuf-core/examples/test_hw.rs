use tuf_core::{AsusDbusClient, BatteryScanner, HwmonScanner};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = AsusDbusClient::new()?;
    println!("Profile: {:?}", client.get_active_profile());
    println!(
        "Custom curves configured: {:?}",
        client.active_custom_fans()
    );
    println!("Graphics: {:?}", client.graphics_summary());
    println!("Charge threshold: {:?}", client.get_charge_limit());
    println!("Panel overdrive: {:?}", client.get_panel_overdrive());
    println!("CPU sustained limit: {:?}", client.get_spl_limit());
    println!("NVIDIA thermal target: {:?}", client.get_nv_temp_target());
    let keyboard = client.keyboard_state();
    println!("Keyboard: {keyboard:?}");
    println!(
        "NVIDIA runtime: {:?}",
        tuf_core::graphics::NvidiaRuntime::read()
    );
    println!("Sensors: {:?}", HwmonScanner::new().read_sensors());
    println!("Battery: {:?}", BatteryScanner::new().read_battery());
    Ok(())
}

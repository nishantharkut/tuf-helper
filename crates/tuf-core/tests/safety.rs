use std::fs;
use tuf_core::{BatteryScanner, HwmonScanner, Profile};

#[test]
fn missing_sensors_are_unknown_but_a_stopped_fan_is_zero() {
    let root = tempfile::tempdir().unwrap();
    let hwmon = root.path().join("hwmon2");
    fs::create_dir(&hwmon).unwrap();
    fs::write(hwmon.join("name"), "asus\n").unwrap();
    fs::write(hwmon.join("fan1_label"), "cpu_fan\n").unwrap();
    fs::write(hwmon.join("fan1_input"), "0\n").unwrap();
    let sensors = HwmonScanner::at(root.path()).read_sensors();
    assert_eq!(sensors.cpu_fan_rpm, Some(0));
    assert_eq!(sensors.gpu_fan_rpm, None);
    assert_eq!(sensors.cpu_temp, None);
}

#[test]
fn battery_is_discovered_by_type_and_discharge_is_negative() {
    let root = tempfile::tempdir().unwrap();
    let battery = root.path().join("CMB0");
    fs::create_dir(&battery).unwrap();
    fs::write(battery.join("type"), "Battery\n").unwrap();
    fs::write(battery.join("status"), "Discharging\n").unwrap();
    fs::write(battery.join("power_now"), "12300000\n").unwrap();
    let data = BatteryScanner::at(root.path()).read_battery().unwrap();
    assert_eq!(data.power_watts, Some(-12.3));
    assert_eq!(data.capacity_percent, None);
    assert_eq!(data.charge_limit_percent, None);
}

#[test]
fn profile_values_match_asusd() {
    assert_eq!(Profile::from_u32(3), Some(Profile::LowPower));
    assert_eq!(Profile::from_u32(4), Some(Profile::Custom));
    assert_eq!(Profile::from_u32(5), None);
}

#[test]
fn battery_health_uses_matching_units_and_reads_external_power() {
    let root = tempfile::tempdir().unwrap();
    let battery = root.path().join("BAT1");
    let ac = root.path().join("ACAD");
    fs::create_dir(&battery).unwrap();
    fs::create_dir(&ac).unwrap();
    for (file, value) in [
        ("type", "Battery"),
        ("charge_full", "4287000"),
        ("charge_full_design", "5675000"),
        ("energy_full", "68000000"),
        ("energy_full_design", "0"),
        ("cycle_count", "0"),
    ] {
        fs::write(battery.join(file), value).unwrap();
    }
    fs::write(ac.join("type"), "Mains").unwrap();
    fs::write(ac.join("online"), "1").unwrap();
    let scanner = BatteryScanner::at(root.path());
    let data = scanner.read_battery().unwrap();
    assert!((data.health_percent.unwrap() - 75.54).abs() < 0.02);
    assert_eq!(data.cycle_count, Some(0));
    assert_eq!(scanner.ac_online(), Some(true));

    fs::write(battery.join("energy_full_design"), "80000000").unwrap();
    assert_eq!(scanner.read_battery().unwrap().health_percent, Some(85.0));
    fs::write(ac.join("online"), "0").unwrap();
    assert_eq!(scanner.ac_online(), Some(false));
    fs::remove_file(ac.join("online")).unwrap();
    assert_eq!(scanner.ac_online(), None);
}

#[test]
fn cpu_threshold_comes_from_the_same_sensor() {
    let root = tempfile::tempdir().unwrap();
    let hwmon = root.path().join("hwmon4");
    fs::create_dir(&hwmon).unwrap();
    fs::write(hwmon.join("name"), "k10temp").unwrap();
    fs::write(hwmon.join("temp1_input"), "85000").unwrap();
    fs::write(hwmon.join("temp2_crit"), "95000").unwrap();
    let scanner = HwmonScanner::at(root.path());
    assert_eq!(scanner.read_sensors().cpu_critical_temp, None);
    fs::write(hwmon.join("temp1_crit"), "100000").unwrap();
    assert_eq!(scanner.read_sensors().cpu_critical_temp, Some(100.0));
}

fn main() {
    uniffi::generate_scaffolding("src/ubm_echo.udl").expect("UDL scaffolding");
}

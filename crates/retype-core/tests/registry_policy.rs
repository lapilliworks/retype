use retype_core::registry::Registry;

#[test]
fn disabling_undo_and_notifications_survives_the_user_config_merge() {
	let user = "version = 'retype-config-1'\n[undo]\nenabled = false\n[notify]\nenabled = false\n";
	let registry = Registry::with_user(user, "user.toml").unwrap();
	assert!(!registry.undo().enabled);
	assert!(!registry.notify().enabled);
}

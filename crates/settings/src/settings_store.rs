use anyhow::{anyhow, Result};
use collections::{hash_map, BTreeMap, HashMap, HashSet};
use schemars::JsonSchema;
use serde::{de::DeserializeOwned, Serialize};
use serde_json::value::RawValue;
use smallvec::SmallVec;
use std::{
    any::{type_name, Any, TypeId},
    cmp::Ordering,
    fmt::Debug,
    mem,
    path::Path,
    sync::Arc,
};
use util::{merge_non_null_json_value_into, ResultExt as _};

/// A value that can be defined as a user setting.
///
/// Settings can be loaded from a combination of multiple JSON files.
pub trait Setting: 'static + Debug {
    /// The name of a field within the JSON file from which this setting should
    /// be deserialized. If this is `None`, then the setting will be deserialized
    /// from the root object.
    const FIELD_NAME: Option<&'static str> = None;

    /// The type that is stored in an individual JSON file.
    type FileContent: DeserializeOwned + JsonSchema;

    /// The logic for combining together values from one or more JSON files into the
    /// final value for this setting.
    ///
    /// The user values are ordered from least specific (the global settings file)
    /// to most specific (the innermost local settings file).
    fn load(default_value: &Self::FileContent, user_values: &[&Self::FileContent]) -> Self;

    fn load_via_json_merge(
        default_value: &Self::FileContent,
        user_values: &[&Self::FileContent],
    ) -> Self
    where
        Self: DeserializeOwned,
        Self::FileContent: Serialize,
    {
        let mut merged = serde_json::Value::Null;
        for value in [default_value].iter().chain(user_values) {
            merge_non_null_json_value_into(serde_json::to_value(value).unwrap(), &mut merged);
        }
        serde_json::from_value(merged).unwrap()
    }
}

/// A set of strongly-typed setting values defined via multiple JSON files.
#[derive(Default)]
pub struct SettingsStore {
    setting_keys: Vec<(Option<&'static str>, TypeId)>,
    setting_values: HashMap<TypeId, Box<dyn AnySettingValue>>,
    default_deserialized_settings: DeserializedSettingMap,
    user_deserialized_settings: Option<DeserializedSettingMap>,
    local_deserialized_settings: BTreeMap<Arc<Path>, DeserializedSettingMap>,
    changed_setting_types: HashSet<TypeId>,
}

#[derive(Debug)]
struct SettingValue<T> {
    global_value: Option<T>,
    local_values: Vec<(Arc<Path>, T)>,
}

trait AnySettingValue: Debug {
    fn setting_type_name(&self) -> &'static str;
    fn deserialize_setting(&self, json: &str) -> Result<DeserializedSetting>;
    fn load_setting(
        &self,
        default_value: &DeserializedSetting,
        custom: &[&DeserializedSetting],
    ) -> Box<dyn Any>;
    fn value_for_path(&self, path: Option<&Path>) -> &dyn Any;
    fn set_global_value(&mut self, value: Box<dyn Any>);
    fn set_local_value(&mut self, path: Arc<Path>, value: Box<dyn Any>);
}

struct DeserializedSetting(Box<dyn Any>);

type DeserializedSettingMap = HashMap<TypeId, DeserializedSetting>;

impl SettingsStore {
    /// Add a new type of setting to the store.
    ///
    /// This should be done before any settings are loaded.
    pub fn register_setting<T: Setting>(&mut self) {
        let type_id = TypeId::of::<T>();

        let entry = self.setting_values.entry(type_id);
        if matches!(entry, hash_map::Entry::Occupied(_)) {
            panic!("duplicate setting type: {}", type_name::<T>());
        }
        entry.or_insert(Box::new(SettingValue::<T> {
            global_value: None,
            local_values: Vec::new(),
        }));

        match self
            .setting_keys
            .binary_search_by_key(&T::FIELD_NAME, |e| e.0)
        {
            Ok(ix) | Err(ix) => self.setting_keys.insert(ix, (T::FIELD_NAME, type_id)),
        }
    }

    /// Get the value of a setting.
    ///
    /// Panics if settings have not yet been loaded, or there is no default
    /// value for this setting.
    pub fn get<T: Setting>(&self, path: Option<&Path>) -> &T {
        self.setting_values
            .get(&TypeId::of::<T>())
            .unwrap()
            .value_for_path(path)
            .downcast_ref::<T>()
            .unwrap()
    }

    /// Set the default settings via a JSON string.
    ///
    /// The string should contain a JSON object with a default value for every setting.
    pub fn set_default_settings(&mut self, default_settings_content: &str) -> Result<()> {
        self.default_deserialized_settings = self.load_setting_map(default_settings_content)?;
        if self.default_deserialized_settings.len() != self.setting_keys.len() {
            return Err(anyhow!(
                "default settings file is missing fields: {:?}",
                self.setting_keys
                    .iter()
                    .filter(|(_, type_id)| !self
                        .default_deserialized_settings
                        .contains_key(type_id))
                    .map(|(name, _)| *name)
                    .collect::<Vec<_>>()
            ));
        }
        self.recompute_values(false, None, None);
        Ok(())
    }

    /// Set the user settings via a JSON string.
    pub fn set_user_settings(&mut self, user_settings_content: &str) -> Result<()> {
        let user_settings = self.load_setting_map(user_settings_content)?;
        let old_user_settings =
            mem::replace(&mut self.user_deserialized_settings, Some(user_settings));
        self.recompute_values(true, None, old_user_settings);
        Ok(())
    }

    /// Add or remove a set of local settings via a JSON string.
    pub fn set_local_settings(
        &mut self,
        path: Arc<Path>,
        settings_content: Option<&str>,
    ) -> Result<()> {
        let removed_map = if let Some(settings_content) = settings_content {
            self.local_deserialized_settings
                .insert(path.clone(), self.load_setting_map(settings_content)?);
            None
        } else {
            self.local_deserialized_settings.remove(&path)
        };
        self.recompute_values(true, Some(&path), removed_map);
        Ok(())
    }

    fn recompute_values(
        &mut self,
        user_settings_changed: bool,
        changed_local_path: Option<&Path>,
        old_settings_map: Option<DeserializedSettingMap>,
    ) {
        // Identify all of the setting types that have changed.
        let new_settings_map = if let Some(changed_path) = changed_local_path {
            &self.local_deserialized_settings.get(changed_path).unwrap()
        } else if user_settings_changed {
            self.user_deserialized_settings.as_ref().unwrap()
        } else {
            &self.default_deserialized_settings
        };
        self.changed_setting_types.clear();
        self.changed_setting_types.extend(new_settings_map.keys());
        if let Some(previous_settings_map) = old_settings_map {
            self.changed_setting_types
                .extend(previous_settings_map.keys());
        }

        // Reload the global and local values for every changed setting.
        let mut user_values_stack = Vec::<&DeserializedSetting>::new();
        for setting_type_id in self.changed_setting_types.iter() {
            let setting_value = self.setting_values.get_mut(setting_type_id).unwrap();

            // Build the prioritized list of deserialized values to pass to the setting's
            // load function.
            user_values_stack.clear();
            if let Some(user_settings) = &self.user_deserialized_settings {
                if let Some(user_value) = user_settings.get(setting_type_id) {
                    user_values_stack.push(&user_value);
                }
            }

            // If the global settings file changed, reload the global value for the field.
            if changed_local_path.is_none() {
                let global_value = setting_value.load_setting(
                    &self.default_deserialized_settings[setting_type_id],
                    &user_values_stack,
                );
                setting_value.set_global_value(global_value);
            }

            // Reload the local values for the setting.
            let user_value_stack_len = user_values_stack.len();
            for (path, deserialized_values) in &self.local_deserialized_settings {
                // If a local settings file changed, then avoid recomputing local
                // settings for any path outside of that directory.
                if changed_local_path.map_or(false, |changed_local_path| {
                    !path.starts_with(changed_local_path)
                }) {
                    continue;
                }

                // Ignore recomputing settings for any path that hasn't customized that setting.
                let Some(deserialized_value) = deserialized_values.get(setting_type_id) else {
                    continue;
                };

                // Build a stack of all of the local values for that setting.
                user_values_stack.truncate(user_value_stack_len);
                for (preceding_path, preceding_deserialized_values) in
                    &self.local_deserialized_settings
                {
                    if preceding_path >= path {
                        break;
                    }
                    if !path.starts_with(preceding_path) {
                        continue;
                    }

                    if let Some(preceding_deserialized_value) =
                        preceding_deserialized_values.get(setting_type_id)
                    {
                        user_values_stack.push(&*preceding_deserialized_value);
                    }
                }
                user_values_stack.push(&*deserialized_value);

                // Load the local value for the field.
                let local_value = setting_value.load_setting(
                    &self.default_deserialized_settings[setting_type_id],
                    &user_values_stack,
                );
                setting_value.set_local_value(path.clone(), local_value);
            }
        }
    }

    /// Deserialize the given JSON string into a map keyed by setting type.
    ///
    /// Returns an error if the string doesn't contain a valid JSON object.
    fn load_setting_map(&self, json: &str) -> Result<DeserializedSettingMap> {
        let mut map = DeserializedSettingMap::default();
        let settings_content_by_key: BTreeMap<&str, &RawValue> = serde_json::from_str(json)?;
        let mut setting_types_by_key = self.setting_keys.iter().peekable();

        // Load all of the fields that don't have a key.
        while let Some((setting_key, setting_type_id)) = setting_types_by_key.peek() {
            if setting_key.is_some() {
                break;
            }
            setting_types_by_key.next();
            if let Some(deserialized_value) = self
                .setting_values
                .get(setting_type_id)
                .unwrap()
                .deserialize_setting(json)
                .log_err()
            {
                map.insert(*setting_type_id, deserialized_value);
            }
        }

        // For each key in the file, load all of the settings that belong to that key.
        for (key, key_content) in settings_content_by_key {
            while let Some((setting_key, setting_type_id)) = setting_types_by_key.peek() {
                let setting_key = setting_key.expect("setting names are ordered");
                match setting_key.cmp(key) {
                    Ordering::Less => {
                        setting_types_by_key.next();
                        continue;
                    }
                    Ordering::Greater => break,
                    Ordering::Equal => {
                        if let Some(deserialized_value) = self
                            .setting_values
                            .get(setting_type_id)
                            .unwrap()
                            .deserialize_setting(key_content.get())
                            .log_err()
                        {
                            map.insert(*setting_type_id, deserialized_value);
                        }
                        setting_types_by_key.next();
                    }
                }
            }
        }
        Ok(map)
    }
}

impl<T: Setting> AnySettingValue for SettingValue<T> {
    fn setting_type_name(&self) -> &'static str {
        type_name::<T>()
    }

    fn load_setting(
        &self,
        default_value: &DeserializedSetting,
        user_values: &[&DeserializedSetting],
    ) -> Box<dyn Any> {
        let default_value = default_value.0.downcast_ref::<T::FileContent>().unwrap();
        let values: SmallVec<[&T::FileContent; 6]> = user_values
            .iter()
            .map(|value| value.0.downcast_ref().unwrap())
            .collect();
        Box::new(T::load(default_value, &values))
    }

    fn deserialize_setting(&self, json: &str) -> Result<DeserializedSetting> {
        let value = serde_json::from_str::<T::FileContent>(json)?;
        Ok(DeserializedSetting(Box::new(value)))
    }

    fn value_for_path(&self, path: Option<&Path>) -> &dyn Any {
        if let Some(path) = path {
            for (settings_path, value) in self.local_values.iter().rev() {
                if path.starts_with(&settings_path) {
                    return value;
                }
            }
        }
        self.global_value.as_ref().unwrap()
    }

    fn set_global_value(&mut self, value: Box<dyn Any>) {
        self.global_value = Some(*value.downcast().unwrap());
    }

    fn set_local_value(&mut self, path: Arc<Path>, value: Box<dyn Any>) {
        let value = *value.downcast().unwrap();
        match self.local_values.binary_search_by_key(&&path, |e| &e.0) {
            Ok(ix) => self.local_values[ix].1 = value,
            Err(ix) => self.local_values.insert(ix, (path, value)),
        }
    }
}

impl Debug for SettingsStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        return f
            .debug_struct("SettingsStore")
            .field(
                "setting_value_sets_by_type",
                &self
                    .setting_values
                    .values()
                    .map(|set| (set.setting_type_name(), set))
                    .collect::<HashMap<_, _>>(),
            )
            .finish_non_exhaustive();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_derive::Deserialize;

    #[test]
    fn test_settings_store() {
        let mut store = SettingsStore::default();
        store.register_setting::<UserSettings>();
        store.register_setting::<TurboSetting>();
        store.register_setting::<MultiKeySettings>();

        // error - missing required field in default settings
        store
            .set_default_settings(
                r#"{
                    "user": {
                        "name": "John Doe",
                        "age": 30,
                        "staff": false
                    }
                }"#,
            )
            .unwrap_err();

        // error - type error in default settings
        store
            .set_default_settings(
                r#"{
                    "turbo": "the-wrong-type",
                    "user": {
                        "name": "John Doe",
                        "age": 30,
                        "staff": false
                    }
                }"#,
            )
            .unwrap_err();

        // valid default settings.
        store
            .set_default_settings(
                r#"{
                    "turbo": false,
                    "user": {
                        "name": "John Doe",
                        "age": 30,
                        "staff": false
                    }
                }"#,
            )
            .unwrap();

        assert_eq!(store.get::<TurboSetting>(None), &TurboSetting(false));
        assert_eq!(
            store.get::<UserSettings>(None),
            &UserSettings {
                name: "John Doe".to_string(),
                age: 30,
                staff: false,
            }
        );
        assert_eq!(
            store.get::<MultiKeySettings>(None),
            &MultiKeySettings {
                key1: String::new(),
                key2: String::new(),
            }
        );

        store
            .set_user_settings(
                r#"{
                    "turbo": true,
                    "user": { "age": 31 },
                    "key1": "a"
                }"#,
            )
            .unwrap();

        assert_eq!(store.get::<TurboSetting>(None), &TurboSetting(true));
        assert_eq!(
            store.get::<UserSettings>(None),
            &UserSettings {
                name: "John Doe".to_string(),
                age: 31,
                staff: false
            }
        );

        store
            .set_local_settings(
                Path::new("/root1").into(),
                Some(r#"{ "user": { "staff": true } }"#),
            )
            .unwrap();
        store
            .set_local_settings(
                Path::new("/root1/subdir").into(),
                Some(r#"{ "user": { "name": "Jane Doe" } }"#),
            )
            .unwrap();

        store
            .set_local_settings(
                Path::new("/root2").into(),
                Some(r#"{ "user": { "age": 42 }, "key2": "b" }"#),
            )
            .unwrap();

        assert_eq!(
            store.get::<UserSettings>(Some(Path::new("/root1/something"))),
            &UserSettings {
                name: "John Doe".to_string(),
                age: 31,
                staff: true
            }
        );
        assert_eq!(
            store.get::<UserSettings>(Some(Path::new("/root1/subdir/something"))),
            &UserSettings {
                name: "Jane Doe".to_string(),
                age: 31,
                staff: true
            }
        );
        assert_eq!(
            store.get::<UserSettings>(Some(Path::new("/root2/something"))),
            &UserSettings {
                name: "John Doe".to_string(),
                age: 42,
                staff: false
            }
        );
        assert_eq!(
            store.get::<MultiKeySettings>(Some(Path::new("/root2/something"))),
            &MultiKeySettings {
                key1: "a".to_string(),
                key2: "b".to_string(),
            }
        );
    }

    #[derive(Debug, PartialEq, Deserialize)]
    struct UserSettings {
        name: String,
        age: u32,
        staff: bool,
    }

    #[derive(Serialize, Deserialize, JsonSchema)]
    struct UserSettingsJson {
        name: Option<String>,
        age: Option<u32>,
        staff: Option<bool>,
    }

    impl Setting for UserSettings {
        const FIELD_NAME: Option<&'static str> = Some("user");
        type FileContent = UserSettingsJson;

        fn load(default_value: &UserSettingsJson, user_values: &[&UserSettingsJson]) -> Self {
            Self::load_via_json_merge(default_value, user_values)
        }
    }

    #[derive(Debug, Deserialize, PartialEq)]
    struct TurboSetting(bool);

    impl Setting for TurboSetting {
        const FIELD_NAME: Option<&'static str> = Some("turbo");
        type FileContent = Option<bool>;

        fn load(default_value: &Option<bool>, user_values: &[&Option<bool>]) -> Self {
            Self::load_via_json_merge(default_value, user_values)
        }
    }

    #[derive(Clone, Debug, PartialEq, Deserialize)]
    struct MultiKeySettings {
        #[serde(default)]
        key1: String,
        #[serde(default)]
        key2: String,
    }

    #[derive(Serialize, Deserialize, JsonSchema)]
    struct MultiKeySettingsJson {
        key1: Option<String>,
        key2: Option<String>,
    }

    impl Setting for MultiKeySettings {
        type FileContent = MultiKeySettingsJson;

        fn load(
            default_value: &MultiKeySettingsJson,
            user_values: &[&MultiKeySettingsJson],
        ) -> Self {
            Self::load_via_json_merge(default_value, user_values)
        }
    }

    #[derive(Debug, Deserialize)]
    struct JournalSettings {
        pub path: String,
        pub hour_format: HourFormat,
    }

    #[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
    #[serde(rename_all = "snake_case")]
    enum HourFormat {
        Hour12,
        Hour24,
    }

    #[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
    struct JournalSettingsJson {
        pub path: Option<String>,
        pub hour_format: Option<HourFormat>,
    }

    impl Setting for JournalSettings {
        const FIELD_NAME: Option<&'static str> = Some("journal");

        type FileContent = JournalSettingsJson;

        fn load(default_value: &JournalSettingsJson, user_values: &[&JournalSettingsJson]) -> Self {
            Self::load_via_json_merge(default_value, user_values)
        }
    }
}

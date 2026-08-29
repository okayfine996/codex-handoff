use std::fmt;

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct ProfileName(String);

impl ProfileName {
    pub fn parse(value: impl Into<String>) -> Result<Self, ProfileNameError> {
        let value = value.into();
        let mut characters = value.chars();
        let Some(first) = characters.next() else {
            return Err(ProfileNameError);
        };

        if !first.is_ascii_alphanumeric()
            || !characters.all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-')
            })
        {
            return Err(ProfileNameError);
        }

        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProfileNameError;

impl fmt::Display for ProfileNameError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("profile names must begin with a letter or number and contain only letters, numbers, '.', '_' or '-'")
    }
}

impl std::error::Error for ProfileNameError {}

#[cfg(test)]
mod tests {
    use super::ProfileName;

    #[test]
    fn accepts_safe_profile_names() {
        assert!(ProfileName::parse("personal.work_1").is_ok());
    }

    #[test]
    fn rejects_path_traversal_profile_names() {
        assert!(ProfileName::parse("../outside").is_err());
        assert!(ProfileName::parse("work/client").is_err());
    }
}

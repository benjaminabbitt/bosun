//! The user who elevated this run (§4.5, §5.5), as the elevating tool names
//! them: sudo sets `SUDO_USER`, `SUDO_UID` and `SUDO_GID`, doas only
//! `DOAS_USER`. Their home comes from the passwd database, never a guess
//! from the name.

use std::path::PathBuf;

/// What the passwd database says about a user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    /// The user id.
    pub uid: u32,
    /// The primary group id.
    pub gid: u32,
    /// The home directory.
    pub home: PathBuf,
}

/// The user who elevated this run: whose files it writes, and as whom.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invoker {
    /// The user name.
    pub name: String,
    /// The user id their files are given.
    pub uid: u32,
    /// The group id their files are given.
    pub gid: u32,
    /// Their home directory.
    pub home: PathBuf,
}

/// The elevating tool named a user the passwd database does not have.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownUser {
    /// The name it gave.
    pub name: String,
}

/// The user who elevated this run, from the elevating tool's variables
/// (`env`) and the passwd database (`lookup`): `None` when no tool names
/// one, or it names root. The ids are sudo's own where it gives them,
/// else the account's.
pub fn invoker(
    env: impl Fn(&str) -> Option<String>,
    lookup: impl Fn(&str) -> Option<Account>,
) -> Result<Option<Invoker>, UnknownUser> {
    let Some(name) = env("SUDO_USER").or_else(|| env("DOAS_USER")) else {
        return Ok(None);
    };
    let Some(account) = lookup(&name) else {
        return Err(UnknownUser { name });
    };
    let id = |var: &str, own: u32| env(var).and_then(|v| v.parse().ok()).unwrap_or(own);
    let uid = id("SUDO_UID", account.uid);
    if uid == 0 {
        return Ok(None);
    }
    Ok(Some(Invoker {
        name,
        uid,
        gid: id("SUDO_GID", account.gid),
        home: account.home,
    }))
}

/// `name`'s passwd entry, by `getpwnam_r`; `None` when there is none or the
/// database cannot be read.
#[cfg(unix)]
pub fn passwd_account(name: &str) -> Option<Account> {
    let user = nix::unistd::User::from_name(name).ok().flatten()?;
    Some(Account {
        uid: user.uid.as_raw(),
        gid: user.gid.as_raw(),
        home: user.dir,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn env<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |var| {
            vars.iter()
                .find(|(k, _)| *k == var)
                .map(|(_, v)| v.to_string())
        }
    }

    fn passwd(name: &str) -> Option<Account> {
        let db = BTreeMap::from([
            ("ben", (1000, 1000, "/srv/ben")),
            ("ann", (1001, 1002, "/home/ann")),
            ("root", (0, 0, "/root")),
        ]);
        db.get(name).map(|(uid, gid, home)| Account {
            uid: *uid,
            gid: *gid,
            home: PathBuf::from(home),
        })
    }

    #[test]
    fn sudo_names_the_user_and_its_ids_are_the_ones_kept() {
        let vars = [
            ("SUDO_USER", "ben"),
            ("SUDO_UID", "1500"),
            ("SUDO_GID", "1600"),
        ];
        assert_eq!(
            invoker(env(&vars), passwd),
            Ok(Some(Invoker {
                name: "ben".to_string(),
                uid: 1500,
                gid: 1600,
                home: PathBuf::from("/srv/ben"),
            }))
        );
    }

    #[test]
    fn ids_sudo_does_not_give_come_from_the_account() {
        let vars = [("SUDO_USER", "ann"), ("SUDO_UID", "x")];
        let user = invoker(env(&vars), passwd).unwrap().unwrap();
        assert_eq!((user.uid, user.gid), (1001, 1002));
    }

    #[test]
    fn doas_names_the_user_and_the_account_gives_the_ids() {
        let user = invoker(env(&[("DOAS_USER", "ann")]), passwd)
            .unwrap()
            .unwrap();
        assert_eq!(user.name, "ann");
        assert_eq!((user.uid, user.gid), (1001, 1002));
        assert_eq!(user.home, PathBuf::from("/home/ann"));
    }

    #[test]
    fn sudo_is_asked_before_doas() {
        let vars = [("SUDO_USER", "ben"), ("DOAS_USER", "ann")];
        assert_eq!(invoker(env(&vars), passwd).unwrap().unwrap().name, "ben");
    }

    #[test]
    fn no_elevating_tool_or_root_itself_names_no_one() {
        assert_eq!(invoker(env(&[]), passwd), Ok(None));
        let vars = [("SUDO_USER", "root"), ("SUDO_UID", "0"), ("SUDO_GID", "0")];
        assert_eq!(invoker(env(&vars), passwd), Ok(None));
        assert_eq!(invoker(env(&[("DOAS_USER", "root")]), passwd), Ok(None));
    }

    #[test]
    fn a_user_the_passwd_database_lacks_is_named() {
        assert_eq!(
            invoker(env(&[("SUDO_USER", "ghost")]), passwd),
            Err(UnknownUser {
                name: "ghost".to_string()
            })
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_passwd_database_has_root_and_not_a_made_up_name() {
        let root = passwd_account("root").expect("root is in passwd");
        assert_eq!((root.uid, root.gid), (0, 0));
        assert!(root.home.is_absolute(), "{:?}", root.home);
        assert_eq!(passwd_account("bosun-no-such-user"), None);
    }
}

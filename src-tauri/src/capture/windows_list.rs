use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ListedWindow {
    pub id: String,
    pub title: String,
    pub pid: u32,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub visible: bool,
    pub owner_is_self: bool,
}

pub fn selectable_windows(windows: &[ListedWindow], self_pid: u32) -> Vec<ListedWindow> {
    windows
        .iter()
        .filter(|window| {
            window.visible
                && !window.owner_is_self
                && window.pid != self_pid
                && window.width > 0
                && window.height > 0
                && !window.title.trim().is_empty()
        })
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn win(id: &str, pid: u32, self_owned: bool, visible: bool, title: &str) -> ListedWindow {
        ListedWindow {
            id: id.into(),
            title: title.into(),
            pid,
            x: 0,
            y: 0,
            width: 800,
            height: 600,
            visible,
            owner_is_self: self_owned,
        }
    }

    #[test]
    fn excludes_own_process_and_hidden_windows() {
        let listed = vec![
            win("a", 11, false, true, "Notes"),
            win("b", 22, true, true, "Cropmark"),
            win("c", 22, false, true, "Preview"),
            win("d", 33, false, false, "Background"),
            win("e", 44, false, true, ""),
            win("f", 55, false, true, "Terminal"),
        ];
        let selected = selectable_windows(&listed, 22);
        let ids: Vec<_> = selected.iter().map(|w| w.id.as_str()).collect();
        assert_eq!(ids, ["a", "f"]);
    }
}

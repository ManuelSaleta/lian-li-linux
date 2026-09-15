use std::collections::VecDeque;

pub(super) fn capacity(requested: u32) -> usize {
    requested.clamp(2, 4096) as usize
}

pub(super) fn seed(history: &mut VecDeque<f32>, requested: u32, minimum: f32, maximum: f32) {
    let cap = capacity(requested);
    let span = (maximum - minimum).abs().max(1.0);
    let base = (minimum + maximum) * 0.5;
    history.clear();
    history.reserve(cap);
    for i in 0..cap {
        let t = i as f32 / (cap - 1) as f32;
        let phase = t * std::f32::consts::PI * 3.0;
        history.push_back(base + span * 0.35 * phase.sin());
    }
}

pub(super) fn push(history: &mut VecDeque<f32>, requested: u32, value: f32) {
    let cap = capacity(requested);
    while history.len() >= cap {
        history.pop_front();
    }
    history.push_back(value);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn excessive_preview_history_stays_bounded_and_live_samples_replace_oldest() {
        let mut history = VecDeque::new();
        seed(&mut history, u32::MAX, 0.0, 100.0);
        assert_eq!(history.len(), 4096);
        assert!(history.iter().all(|value| (15.0..=85.0).contains(value)));
        let allocated = history.capacity();
        for value in 0..5000 {
            push(&mut history, u32::MAX, value as f32);
        }
        assert_eq!(history.len(), 4096);
        assert_eq!(history.front(), Some(&904.0));
        assert_eq!(history.back(), Some(&4999.0));
        assert_eq!(history.capacity(), allocated);
        push(&mut history, 0, 7.0);
        assert_eq!(history.into_iter().collect::<Vec<_>>(), vec![4999.0, 7.0]);
    }
}

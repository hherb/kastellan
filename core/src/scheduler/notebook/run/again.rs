//! The `failed_step` code for a call that was not run (#956), moved
//! verbatim out of `run.rs` (over the 500-line cap, #1044).

use crate::scheduler::inner_loop::execute;
use crate::scheduler::notebook::sift;

/// The `failed_step` code for a call that was not run: a refused sift or a
/// rejected invoke says so, rather than reading as a reviewer block (#956).
/// `blocks` are the reasons this call pushed. Pure.
pub(super) fn again_code(blocks: &[String]) -> &'static str {
    let all = |prefix: &str| !blocks.is_empty() && blocks.iter().all(|b| b.starts_with(prefix));
    if all(sift::run::SIFT_REFUSED_BLOCK) {
        "SIFT_REFUSED"
    } else if all(execute::INVOKE_REJECTED_BLOCK) {
        "INVOKE_REJECTED"
    } else {
        "BLOCKED"
    }
}

#[cfg(test)]
mod again_code_tests {
    /// #956: the code says why a call was not run; an empty or mixed set of
    /// reasons stays a reviewer block.
    #[test]
    fn the_failed_step_code_names_why() {
        let b = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(super::again_code(&b(&[])), "BLOCKED");
        assert_eq!(super::again_code(&b(&["forbidden tool"])), "BLOCKED");
        assert_eq!(super::again_code(&b(&["sift_refused: no list"])), "SIFT_REFUSED");
        assert_eq!(super::again_code(&b(&["invoke_rejected: a", "invoke_rejected: b"])), "INVOKE_REJECTED");
        assert_eq!(super::again_code(&b(&["sift_refused: no list", "forbidden tool"])), "BLOCKED");
    }
}

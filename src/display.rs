//! Human-readable output for `nixit --dry-run`.

use colored::Colorize;

use crate::plan::RepoPlan;

/// Print the plan, grouped by repository. Repositories with no differences are
/// omitted so the output stays focused.
pub fn print_plan(plans: &[RepoPlan]) {
    let mut any = false;

    for plan in plans {
        if !plan.has_changes() {
            continue;
        }
        any = true;
        let target = format!("{}/{}", plan.owner, plan.repo);
        if plan.exists {
            println!("\n{} {}", "~".yellow().bold(), target.bold());
        } else {
            println!("\n{} {}", "+".green().bold(), target.bold());
        }
        for change in &plan.changes {
            let from = change.from.as_deref().unwrap_or("(unset)");
            let path = if change.scope.is_empty() {
                change.field.clone()
            } else {
                format!("{}.{}", change.scope, change.field)
            };
            println!(
                "    {}: {} {} {}",
                path,
                from.dimmed(),
                "->".dimmed(),
                change.to.green()
            );
        }
    }

    if any {
        println!();
    } else {
        println!("\n{}", "No changes.".green());
    }
}

use std::{
    collections::{HashMap, HashSet},
    os::unix::{fs::PermissionsExt, prelude::OsStrExt},
    path, time, vec,
};

use anyhow::{anyhow, bail, Context, Result};
use diffy::{apply_bytes, Patch};
use serde::Serialize;
use slug::slugify;

use crate::{
    dedup::{dedup, dedup_fmt},
    gb_repository,
    git::{self, diff, RemoteBranchName},
    keys,
    project_repository::{self, conflicts, LogUntil},
    reader, sessions, users,
};

use super::{
    branch::{self, Branch, BranchCreateRequest, BranchId, FileOwnership, Hunk, Ownership},
    branch_to_remote_branch, target, Iterator, RemoteBranch,
};

type AppliedStatuses = Vec<(branch::Branch, HashMap<path::PathBuf, Vec<diff::Hunk>>)>;

// this struct is a mapping to the view `Branch` type in Typescript
// found in src-tauri/src/routes/repo/[project_id]/types.ts
// it holds a materialized view for presentation purposes of the Branch struct in Rust
// which is our persisted data structure for virtual branches
//
// it is not persisted, it is only used for presentation purposes through the ipc
//
#[derive(Debug, PartialEq, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
#[allow(clippy::struct_excessive_bools)]
pub struct VirtualBranch {
    pub id: BranchId,
    pub name: String,
    pub notes: String,
    pub active: bool,
    pub files: Vec<VirtualBranchFile>,
    pub commits: Vec<VirtualBranchCommit>,
    pub requires_force: bool, // does this branch require a force push to the upstream?
    pub conflicted: bool, // is this branch currently in a conflicted state (only for applied branches)
    pub order: usize,     // the order in which this branch should be displayed in the UI
    pub upstream: Option<RemoteBranch>, // the upstream branch where this branch pushes to, if any
    pub base_current: bool, // is this vbranch based on the current base branch? if false, this needs to be manually merged with conflicts
    pub ownership: Ownership,
}

// this is the struct that maps to the view `Commit` type in Typescript
// it is derived from walking the git commits between the `Branch.head` commit
// and the `Target.sha` commit, or, everything that is uniquely committed to
// the virtual branch we assign it to. an array of them are returned as part of
// the `VirtualBranch` struct
//
// it is not persisted, it is only used for presentation purposes through the ipc
//
#[derive(Debug, PartialEq, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VirtualBranchCommit {
    pub id: git::Oid,
    pub description: String,
    pub created_at: u128,
    pub author: Author,
    pub is_remote: bool,
    pub files: Vec<VirtualBranchFile>,
    pub is_integrated: bool,
}

// this struct is a mapping to the view `File` type in Typescript
// found in src-tauri/src/routes/repo/[project_id]/types.ts
// it holds a materialized view for presentation purposes of one entry of the
// `Branch.ownership` vector in Rust. an array of them are returned as part of
// the `VirtualBranch` struct, which map to each entry of the `Branch.ownership` vector
//
// it is not persisted, it is only used for presentation purposes through the ipc
//
#[derive(Debug, PartialEq, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VirtualBranchFile {
    pub id: String,
    pub path: path::PathBuf,
    pub hunks: Vec<VirtualBranchHunk>,
    pub modified_at: u128,
    pub conflicted: bool,
    pub binary: bool,
}

// this struct is a mapping to the view `Hunk` type in Typescript
// found in src-tauri/src/routes/repo/[project_id]/types.ts
// it holds a materialized view for presentation purposes of one entry of the
// each hunk in one `Branch.ownership` vector entry in Rust.
// an array of them are returned as part of the `VirtualBranchFile` struct
//
// it is not persisted, it is only used for presentation purposes through the ipc
//
#[derive(Debug, PartialEq, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VirtualBranchHunk {
    pub id: String,
    pub diff: String,
    pub modified_at: u128,
    pub file_path: path::PathBuf,
    pub hash: String,
    pub start: u32,
    pub end: u32,
    pub binary: bool,
    pub locked: bool,
}

#[derive(Debug, Serialize, Hash, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Author {
    pub name: String,
    pub email: String,
    pub gravatar_url: url::Url,
}

impl From<git::Signature<'_>> for Author {
    fn from(value: git::Signature) -> Self {
        let name = value.name().unwrap_or_default().to_string();
        let email = value.email().unwrap_or_default().to_string();

        let gravatar_url = url::Url::parse(&format!(
            "https://www.gravatar.com/avatar/{:x}?s=100&r=g&d=retro",
            md5::compute(email.to_lowercase())
        ))
        .unwrap();

        Author {
            name,
            email,
            gravatar_url,
        }
    }
}

pub fn get_default_target(
    current_session_reader: &sessions::Reader,
) -> Result<Option<target::Target>> {
    let target_reader = target::Reader::new(current_session_reader);
    match target_reader.read_default() {
        Ok(target) => Ok(Some(target)),
        Err(reader::Error::NotFound) => Ok(None),
        Err(e) => Err(e).context("failed to read default target"),
    }
}

pub fn apply_branch(
    gb_repository: &gb_repository::Repository,
    project_repository: &project_repository::Repository,
    branch_id: &BranchId,
    signing_key: Option<&keys::PrivateKey>,
    user: Option<&users::User>,
) -> Result<()> {
    if conflicts::is_resolving(project_repository) {
        bail!("cannot apply a branch, project is in a conflicted state");
    }
    let current_session = gb_repository
        .get_or_create_current_session()
        .context("failed to get or create currnt session")?;
    let current_session_reader = sessions::Reader::open(gb_repository, &current_session)
        .context("failed to open current session")?;

    let repo = &project_repository.git_repository;

    let default_target = match get_default_target(&current_session_reader)
        .context("failed to get default target")?
    {
        Some(target) => target,
        None => return Ok(()),
    };

    let writer = branch::Writer::new(gb_repository);

    let mut apply_branch = branch::Reader::new(&current_session_reader)
        .read(branch_id)
        .context(format!("failed to read branch {}", branch_id))?;

    let target_commit = repo
        .find_commit(default_target.sha)
        .context("failed to find target commit")?;
    let target_tree = target_commit.tree().context("failed to get target tree")?;

    let mut branch_tree = repo
        .find_tree(apply_branch.tree)
        .context("failed to find branch tree")?;

    // calculate the merge base and make sure it's the same as the target commit
    // if not, we need to merge or rebase the branch to get it up to date

    let merge_base = repo.merge_base(default_target.sha, apply_branch.head)?;
    if merge_base != default_target.sha {
        // Branch is out of date, merge or rebase it
        let merge_base_tree = repo.find_commit(merge_base)?.tree()?;
        let mut merge_index = repo
            .merge_trees(&merge_base_tree, &branch_tree, &target_tree)
            .context("failed to merge trees")?;

        if merge_index.has_conflicts() {
            // currently we can only deal with the merge problem branch
            unapply_all_branches(gb_repository, project_repository)?;

            // apply the branch
            apply_branch.applied = true;
            writer.write(&apply_branch)?;

            // checkout the conflicts
            repo.checkout_index(&mut merge_index)
                .allow_conflicts()
                .conflict_style_merge()
                .force()
                .checkout()?;

            // mark conflicts
            let conflicts = merge_index.conflicts()?;
            let mut merge_conflicts = Vec::new();
            for path in conflicts.flatten() {
                if let Some(ours) = path.our {
                    let path = std::str::from_utf8(&ours.path)?.to_string();
                    merge_conflicts.push(path);
                }
            }
            conflicts::mark(
                project_repository,
                &merge_conflicts,
                Some(default_target.sha),
            )?;
            return Ok(());
        }

        let head_commit = repo
            .find_commit(apply_branch.head)
            .context("failed to find head commit")?;

        // commit our new upstream merge
        let (author, committer) = project_repository.git_signatures(user)?;
        let message = "merge upstream";
        // write the merge commit
        let branch_tree_oid = merge_index.write_tree_to(repo)?;
        branch_tree = repo.find_tree(branch_tree_oid)?;

        let new_branch_head = if let Some(key) = signing_key {
            repo.commit_signed(
                &author,
                message,
                &branch_tree,
                &[&head_commit, &target_commit],
                key,
            )
        } else {
            repo.commit(
                None,
                &author,
                &committer,
                message,
                &branch_tree,
                &[&head_commit, &target_commit],
            )
        }?;

        // ok, update the virtual branch
        apply_branch.head = new_branch_head;
        apply_branch.tree = branch_tree_oid;
        writer.write(&apply_branch)?;
    }

    let wd_tree = project_repository.get_wd_tree()?;

    // check index for conflicts
    let mut merge_index = repo
        .merge_trees(&target_tree, &wd_tree, &branch_tree)
        .context("failed to merge trees")?;

    if merge_index.has_conflicts() {
        bail!("vbranch has conflicts with other applied branches, sorry bro.");
    }

    // apply the branch
    apply_branch.applied = true;
    writer.write(&apply_branch)?;

    // checkout the merge index
    repo.checkout_index(&mut merge_index).force().checkout()?;

    super::integration::update_gitbutler_integration(gb_repository, project_repository)?;

    Ok(())
}

pub fn unapply_ownership(
    gb_repository: &gb_repository::Repository,
    project_repository: &project_repository::Repository,
    ownership: &Ownership,
) -> Result<()> {
    if conflicts::is_resolving(project_repository) {
        bail!("cannot unapply, project is in a conflicted state");
    }
    let current_session = gb_repository
        .get_or_create_current_session()
        .context("failed to get or create currnt session")?;
    let current_session_reader = sessions::Reader::open(gb_repository, &current_session)
        .context("failed to open current session")?;

    let default_target = match get_default_target(&current_session_reader)
        .context("failed to get default target")?
    {
        Some(target) => target,
        None => return Ok(()),
    };

    let applied_branches = Iterator::new(&current_session_reader)
        .context("failed to create branch iterator")?
        .collect::<Result<Vec<branch::Branch>, reader::Error>>()
        .context("failed to read virtual branches")?
        .into_iter()
        .filter(|b| b.applied)
        .collect::<Vec<_>>();

    let applied_statuses = get_applied_status(
        gb_repository,
        project_repository,
        &default_target,
        applied_branches,
    )
    .context("failed to get status by branch")?;

    // remove the ownership from the applied branches, and write them out
    let branch_writer = branch::Writer::new(gb_repository);
    let applied_statuses = applied_statuses
        .into_iter()
        .map(|(branch, branch_files)| {
            let mut branch = branch.clone();
            let mut branch_files = branch_files.clone();
            for file_ownership_to_take in &ownership.files {
                let taken_file_ownerships = branch.ownership.take(file_ownership_to_take);
                if taken_file_ownerships.is_empty() {
                    continue;
                }
                branch_writer.write(&branch)?;
                branch_files = branch_files
                    .iter_mut()
                    .filter_map(|(filepath, hunks)| {
                        let hunks = hunks
                            .clone()
                            .into_iter()
                            .filter(|hunk| {
                                !taken_file_ownerships.iter().any(|taken| {
                                    taken.file_path.eq(filepath)
                                        && taken.hunks.iter().any(|taken_hunk| {
                                            taken_hunk.start == hunk.new_start
                                                && taken_hunk.end == hunk.new_start + hunk.new_lines
                                        })
                                })
                            })
                            .collect::<Vec<_>>();
                        if hunks.is_empty() {
                            None
                        } else {
                            Some((filepath.clone(), hunks))
                        }
                    })
                    .collect::<HashMap<_, _>>();
            }
            Ok((branch, branch_files))
        })
        .collect::<Result<Vec<_>>>()?;

    let repo = &project_repository.git_repository;

    let target_commit = repo
        .find_commit(default_target.sha)
        .context("failed to find target commit")?;

    // ok, update the wd with the union of the rest of the branches
    let base_tree = target_commit.tree()?;

    // construst a new working directory tree, without the removed ownerships
    let final_tree = applied_statuses.into_iter().fold(
        target_commit.tree().context("failed to get target tree"),
        |final_tree, status| {
            let final_tree = final_tree?;
            let tree_oid = write_tree(project_repository, &default_target, &status.1)?;
            let branch_tree = repo.find_tree(tree_oid)?;
            let mut result = repo.merge_trees(&base_tree, &final_tree, &branch_tree)?;
            let final_tree_oid = result.write_tree_to(repo)?;
            repo.find_tree(final_tree_oid)
                .context("failed to find tree")
        },
    )?;

    repo.checkout_tree(&final_tree)
        .force()
        .remove_untracked()
        .checkout()?;

    super::integration::update_gitbutler_integration(gb_repository, project_repository)?;

    Ok(())
}

// to unapply a branch, we need to write the current tree out, then remove those file changes from the wd
pub fn unapply_branch(
    gb_repository: &gb_repository::Repository,
    project_repository: &project_repository::Repository,
    branch_id: &BranchId,
) -> Result<()> {
    if conflicts::is_resolving(project_repository) {
        bail!("cannot unapply, project is in a conflicted state");
    }

    let (default_target, applied_statuses) = if let Some(result) = {
        let session = &gb_repository
            .get_or_create_current_session()
            .context("failed to get or create currnt session")?;

        let current_session_reader = sessions::Reader::open(gb_repository, session)
            .context("failed to open current session")?;

        let branch_reader = branch::Reader::new(&current_session_reader);

        let mut target_branch = branch_reader
            .read(branch_id)
            .context("failed to read branch")?;

        if !target_branch.applied {
            bail!("branch is not applied");
        }

        target_branch.applied = false;

        flush_vbranch_as_tree(gb_repository, project_repository, session, target_branch)?
    } {
        result
    } else {
        return Ok(());
    };

    let repo = &project_repository.git_repository;

    let target_commit = repo
        .find_commit(default_target.sha)
        .context("failed to find target commit")?;

    // ok, update the wd with the union of the rest of the branches
    let base_tree = target_commit.tree()?;

    // go through the other applied branches and merge them into the final tree
    // then check that out into the working directory
    let final_tree = applied_statuses
        .into_iter()
        .filter(|(branch, _)| &branch.id != branch_id)
        .fold(
            target_commit.tree().context("failed to get target tree"),
            |final_tree, status| {
                let final_tree = final_tree?;
                let tree_oid = write_tree(project_repository, &default_target, &status.1)?;
                let branch_tree = repo.find_tree(tree_oid)?;
                let mut result = repo.merge_trees(&base_tree, &final_tree, &branch_tree)?;
                let final_tree_oid = result.write_tree_to(repo)?;
                repo.find_tree(final_tree_oid)
                    .context("failed to find tree")
            },
        )?;

    // checkout final_tree into the working directory
    repo.checkout_tree(&final_tree)
        .force()
        .remove_untracked()
        .checkout()?;

    super::integration::update_gitbutler_integration(gb_repository, project_repository)?;

    Ok(())
}

fn flush_vbranch_as_tree(
    gb_repository: &gb_repository::Repository,
    project_repository: &project_repository::Repository,
    session: &sessions::Session,
    branch: Branch,
) -> Result<Option<(target::Target, AppliedStatuses)>> {
    let branch_writer = branch::Writer::new(gb_repository);

    let current_session_reader =
        sessions::Reader::open(gb_repository, session).context("failed to open current session")?;

    let default_target = match get_default_target(&current_session_reader)
        .context("failed to get default target")?
    {
        Some(target) => target,
        None => return Ok(None),
    };

    let applied_branches = Iterator::new(&current_session_reader)
        .context("failed to create branch iterator")?
        .collect::<Result<Vec<branch::Branch>, reader::Error>>()
        .context("failed to read virtual branches")?
        .into_iter()
        .filter(|b| b.applied)
        .collect::<Vec<_>>();

    let applied_statuses = get_applied_status(
        gb_repository,
        project_repository,
        &default_target,
        applied_branches,
    )
    .context("failed to get status by branch")?;

    let status = applied_statuses
        .iter()
        .find(|(s, _)| s.id == branch.id)
        .context("failed to find status for branch");

    if let Ok((_, files)) = status {
        let tree = write_tree(project_repository, &default_target, files)?;

        let mut branch = branch;
        branch.tree = tree;
        branch_writer.write(&branch)?;
    }

    Ok(Some((default_target, applied_statuses)))
}

fn unapply_all_branches(
    gb_repository: &gb_repository::Repository,
    project_repository: &project_repository::Repository,
) -> Result<()> {
    let applied_virtual_branches = list_applied_vbranches(gb_repository)?;

    for branch in applied_virtual_branches {
        let branch_id = branch.id;
        unapply_branch(gb_repository, project_repository, &branch_id)
            .context("failed to unapply branch")?;
    }

    Ok(())
}

pub fn flush_applied_vbranches(
    gb_repository: &gb_repository::Repository,
    project_repository: &project_repository::Repository,
) -> Result<()> {
    let applied_branches = list_applied_vbranches(gb_repository)?;

    let session = &gb_repository
        .get_or_create_current_session()
        .context("failed to get or create currnt session")?;

    let current_session_reader =
        sessions::Reader::open(gb_repository, session).context("failed to open current session")?;

    let default_target = match get_default_target(&current_session_reader)
        .context("failed to get default target")?
    {
        Some(target) => target,
        None => return Ok(()),
    };

    let applied_statuses = get_applied_status(
        gb_repository,
        project_repository,
        &default_target,
        applied_branches,
    )
    .context("failed to get status by branch")?;

    let branch_writer = branch::Writer::new(gb_repository);

    for (b, files) in applied_statuses {
        if b.applied {
            let tree = write_tree(project_repository, &default_target, &files)?;
            let mut branch = b;
            branch.tree = tree;
            branch_writer.write(&branch)?;
        }
    }

    super::integration::update_gitbutler_integration(gb_repository, project_repository)?;

    Ok(())
}

fn list_applied_vbranches(
    gb_repository: &gb_repository::Repository,
) -> Result<Vec<Branch>, anyhow::Error> {
    let current_session = gb_repository
        .get_or_create_current_session()
        .context("failed to get or create currnt session")?;
    let current_session_reader = sessions::Reader::open(gb_repository, &current_session)
        .context("failed to open current session")?;
    let applied_virtual_branches = Iterator::new(&current_session_reader)
        .context("failed to create branch iterator")?
        .collect::<Result<Vec<branch::Branch>, reader::Error>>()
        .context("failed to read virtual branches")?
        .into_iter()
        .filter(|branch| branch.applied)
        .collect::<Vec<_>>();
    Ok(applied_virtual_branches)
}

fn find_base_tree<'a>(
    repo: &'a git::Repository,
    branch_commit: &'a git::Commit<'a>,
    target_commit: &'a git::Commit<'a>,
) -> Result<git::Tree<'a>> {
    // find merge base between target_commit and branch_commit
    let merge_base = repo
        .merge_base(target_commit.id(), branch_commit.id())
        .context("failed to find merge base")?;
    // turn oid into a commit
    let merge_base_commit = repo
        .find_commit(merge_base)
        .context("failed to find merge base commit")?;
    let base_tree = merge_base_commit
        .tree()
        .context("failed to get base tree object")?;
    Ok(base_tree)
}

pub fn list_virtual_branches(
    gb_repository: &gb_repository::Repository,
    project_repository: &project_repository::Repository,
) -> Result<Vec<VirtualBranch>> {
    let mut branches: Vec<VirtualBranch> = Vec::new();
    let current_session = gb_repository
        .get_or_create_current_session()
        .context("failed to get or create currnt session")?;

    let current_session_reader = sessions::Reader::open(gb_repository, &current_session)
        .context("failed to open current session reader")?;

    let default_target = match get_default_target(&current_session_reader)
        .context("failed to get default target")?
    {
        Some(target) => target,
        None => return Ok(vec![]),
    };

    let statuses = get_status_by_branch(gb_repository, project_repository)?;
    for (branch, files) in &statuses {
        let file_diffs = files
            .iter()
            .map(|(filepath, hunks)| {
                (
                    filepath,
                    hunks.iter().map(|hunk| &hunk.diff).collect::<HashSet<_>>(),
                )
            })
            .collect::<HashMap<_, _>>();

        // check if head tree does not match target tree
        // if so, we diff the head tree and the new write_tree output to see what is new and filter the hunks to just those
        let files =
            calculate_non_commited_diffs(project_repository, branch, &default_target, files)?;

        let repo = &project_repository.git_repository;

        let upstream_branch = match branch
            .upstream
            .as_ref()
            .map(|name| repo.find_branch(&git::BranchName::from(name.clone())))
            .transpose()
        {
            Err(git::Error::NotFound(_)) => Ok(None),
            Err(error) => Err(error),
            Ok(branch) => Ok(branch),
        }
        .context(format!(
            "failed to find upstream branch for {}",
            branch.name
        ))?;

        let upstram_branch_commit = upstream_branch
            .as_ref()
            .map(git::Branch::peel_to_commit)
            .transpose()?;

        // find upstream commits if we found an upstream reference
        let mut pushed_commits = HashMap::new();
        if let Some(upstream) = &upstram_branch_commit {
            let merge_base =
                repo.merge_base(upstream.id(), default_target.sha)
                    .context(format!(
                        "failed to find merge base between {} and {}",
                        upstream.id(),
                        default_target.sha
                    ))?;
            for oid in project_repository.l(upstream.id(), LogUntil::Commit(merge_base))? {
                pushed_commits.insert(oid, true);
            }
        }

        // find all commits on head that are not on target.sha
        let commits = project_repository
            .log(branch.head, LogUntil::Commit(default_target.sha))
            .context(format!("failed to get log for branch {}", branch.name))?
            .iter()
            .map(|commit| {
                commit_to_vbranch_commit(
                    project_repository,
                    &default_target,
                    commit,
                    Some(&pushed_commits),
                )
            })
            .collect::<Result<Vec<_>>>()?;

        // if the branch is not applied, check to see if it's mergeable and up to date
        let mut base_current = true;
        if !branch.applied {
            // determine if this branch is up to date with the target/base
            let merge_base = repo.merge_base(default_target.sha, branch.head)?;
            if merge_base != default_target.sha {
                base_current = false;
            }
        }

        let upstream = upstream_branch
            .map(|upstream_branch| {
                branch_to_remote_branch(project_repository, &upstream_branch, branch.head)
            })
            .transpose()?
            .flatten();

        let mut files = diffs_to_virtual_files(project_repository, &files);
        files.sort_by(|a, b| {
            branch
                .ownership
                .files
                .iter()
                .position(|o| o.file_path.eq(&a.path))
                .unwrap_or(999)
                .cmp(
                    &branch
                        .ownership
                        .files
                        .iter()
                        .position(|id| id.file_path.eq(&b.path))
                        .unwrap_or(999),
                )
        });

        // mark locked hunks
        for file in &mut files {
            file.hunks.iter_mut().for_each(|hunk| {
                // we consider a hunk to be locked if it's not seen verbatim
                // non-commited. reason beging - we can't partialy move hunks between
                // branches just yet.
                hunk.locked = file_diffs
                    .get(&file.path)
                    .map_or(false, |h| !h.contains(&hunk.diff));
            });
        }

        let requires_force = is_requires_force(project_repository, branch)?;
        let branch = VirtualBranch {
            id: branch.id,
            name: branch.name.clone(),
            notes: branch.notes.clone(),
            active: branch.applied,
            files,
            order: branch.order,
            commits,
            requires_force,
            upstream,
            conflicted: conflicts::is_resolving(project_repository),
            base_current,
            ownership: branch.ownership.clone(),
        };
        branches.push(branch);
    }
    branches.sort_by(|a, b| a.order.cmp(&b.order));
    Ok(branches)
}

fn is_requires_force(
    project_repository: &project_repository::Repository,
    branch: &branch::Branch,
) -> Result<bool> {
    let upstream = if let Some(upstream) = &branch.upstream {
        upstream
    } else {
        return Ok(false);
    };

    let reference = match project_repository
        .git_repository
        .refname_to_id(&upstream.to_string())
    {
        Ok(reference) => reference,
        Err(git::Error::NotFound(_)) => return Ok(false),
        Err(other) => return Err(other).context("failed to find upstream reference"),
    };

    let upstream_commit = project_repository
        .git_repository
        .find_commit(reference)
        .context("failed to find upstream commit")?;

    let merge_base = project_repository
        .git_repository
        .merge_base(upstream_commit.id(), branch.head)?;

    Ok(merge_base != upstream_commit.id())
}

// given a virtual branch and it's files that are calculated off of a default target,
// return files adjusted to the branch's head commit
fn calculate_non_commited_diffs(
    project_repository: &project_repository::Repository,
    branch: &branch::Branch,
    default_target: &target::Target,
    files: &HashMap<path::PathBuf, Vec<diff::Hunk>>,
) -> Result<HashMap<path::PathBuf, Vec<diff::Hunk>>> {
    if default_target.sha == branch.head && !branch.applied {
        return Ok(files.clone());
    };

    // get the trees
    let target_plus_wd_oid = write_tree(project_repository, default_target, files)?;
    let target_plus_wd = project_repository
        .git_repository
        .find_tree(target_plus_wd_oid)?;
    let branch_head = project_repository
        .git_repository
        .find_commit(branch.head)?
        .tree()?;

    // do a diff between branch.head and the tree we _would_ commit
    let non_commited_diff = diff::trees(
        &project_repository.git_repository,
        &branch_head,
        &target_plus_wd,
    )
    .context("failed to diff trees")?;

    // record conflicts resolution
    // TODO: this feels out of place. move it somewhere else?
    let conflicting_files = conflicts::conflicting_files(project_repository)?;
    for (file_path, non_commited_hunks) in &non_commited_diff {
        let mut conflicted = false;
        if let Some(conflicts) = &conflicting_files {
            if conflicts.contains(&file_path.display().to_string()) {
                // check file for conflict markers, resolve the file if there are none in any hunk
                for hunk in non_commited_hunks {
                    if hunk.diff.contains("<<<<<<< ours") {
                        conflicted = true;
                    }
                    if hunk.diff.contains(">>>>>>> theirs") {
                        conflicted = true;
                    }
                }
                if !conflicted {
                    conflicts::resolve(project_repository, &file_path.display().to_string())
                        .unwrap();
                }
            }
        }
    }

    Ok(non_commited_diff)
}

fn list_virtual_commit_files(
    project_repository: &project_repository::Repository,
    commit: &git::Commit,
) -> Result<Vec<VirtualBranchFile>> {
    if commit.parent_count() == 0 {
        return Ok(vec![]);
    }
    let parent = commit.parent(0).context("failed to get parent commit")?;
    let commit_tree = commit.tree().context("failed to get commit tree")?;
    let parent_tree = parent.tree().context("failed to get parent tree")?;
    let diff = diff::trees(
        &project_repository.git_repository,
        &parent_tree,
        &commit_tree,
    )?;
    let hunks_by_filepath = virtual_hunks_by_filepath(&project_repository.git_repository, &diff);
    Ok(virtual_hunks_to_virtual_files(
        project_repository,
        &hunks_by_filepath
            .values()
            .flatten()
            .cloned()
            .collect::<Vec<_>>(),
    ))
}

pub fn commit_to_vbranch_commit(
    repository: &project_repository::Repository,
    target: &target::Target,
    commit: &git::Commit,
    upstream_commits: Option<&HashMap<git::Oid, bool>>,
) -> Result<VirtualBranchCommit> {
    let timestamp = u128::try_from(commit.time().seconds())?;
    let signature = commit.author();
    let message = commit.message().unwrap().to_string();

    let is_remote = match upstream_commits {
        Some(commits) => commits.contains_key(&commit.id()),
        None => true,
    };

    let files =
        list_virtual_commit_files(repository, commit).context("failed to list commit files")?;

    let is_integrated = is_commit_integrated(repository, target, commit)?;

    let commit = VirtualBranchCommit {
        id: commit.id(),
        created_at: timestamp * 1000,
        author: Author::from(signature),
        description: message,
        is_remote,
        files,
        is_integrated,
    };

    Ok(commit)
}

pub fn create_virtual_branch(
    gb_repository: &gb_repository::Repository,
    project_repository: &project_repository::Repository,
    create: &BranchCreateRequest,
) -> Result<branch::Branch> {
    let current_session = gb_repository
        .get_or_create_current_session()
        .context("failed to get or create currnt session")?;
    let current_session_reader = sessions::Reader::open(gb_repository, &current_session)
        .context("failed to open current session")?;

    let target_reader = target::Reader::new(&current_session_reader);
    let default_target = target_reader
        .read_default()
        .context("failed to read default")?;

    let repo = gb_repository.git_repository();
    let commit = repo
        .find_commit(default_target.sha)
        .context("failed to find commit")?;
    let tree = commit.tree().context("failed to find tree")?;

    let mut all_virtual_branches = Iterator::new(&current_session_reader)
        .context("failed to create branch iterator")?
        .collect::<Result<Vec<branch::Branch>, reader::Error>>()
        .context("failed to read virtual branches")?
        .into_iter()
        .collect::<Vec<branch::Branch>>();
    all_virtual_branches.sort_by_key(|branch| branch.order);

    let order = create
        .order
        .unwrap_or(all_virtual_branches.len())
        .clamp(0, all_virtual_branches.len());

    let branch_writer = branch::Writer::new(gb_repository);

    // make space for the new branch
    for (i, branch) in all_virtual_branches.iter().enumerate() {
        let mut branch = branch.clone();
        let new_order = if i < order { i } else { i + 1 };
        if branch.order != new_order {
            branch.order = new_order;
            branch_writer
                .write(&branch)
                .context("failed to write branch")?;
        }
    }

    let now = time::UNIX_EPOCH
        .elapsed()
        .context("failed to get elapsed time")?
        .as_millis();

    let name = dedup(
        &all_virtual_branches
            .iter()
            .map(|b| b.name.as_str())
            .collect::<Vec<_>>(),
        create
            .name
            .as_ref()
            .unwrap_or(&"Virtual branch".to_string()),
    );

    let mut branch = Branch {
        id: BranchId::generate(),
        name,
        notes: String::new(),
        applied: true,
        upstream: None,
        upstream_head: None,
        tree: tree.id(),
        head: default_target.sha,
        created_timestamp_ms: now,
        updated_timestamp_ms: now,
        ownership: Ownership::default(),
        order,
    };

    if let Some(ownership) = &create.ownership {
        let branch_reader = branch::Reader::new(&current_session_reader);
        set_ownership(&branch_reader, &branch_writer, &mut branch, ownership)
            .context("failed to set ownership")?;
    }

    branch_writer
        .write(&branch)
        .context("failed to write branch")?;

    project_repository.add_branch_reference(&branch)?;

    Ok(branch)
}

pub fn merge_virtual_branch_upstream(
    gb_repository: &gb_repository::Repository,
    project_repository: &project_repository::Repository,
    branch_id: &BranchId,
    signing_key: Option<&keys::PrivateKey>,
    user: Option<&users::User>,
) -> Result<()> {
    if conflicts::is_conflicting(project_repository, None)? {
        bail!("cannot merge upstream, project is in a conflicted state");
    }
    let current_session = gb_repository
        .get_or_create_current_session()
        .context("failed to get current session")?;
    let current_session_reader = sessions::Reader::open(gb_repository, &current_session)
        .context("failed to open current session")?;

    // get the branch
    let branch_reader = branch::Reader::new(&current_session_reader);
    let mut branch = branch_reader
        .read(branch_id)
        .context("failed to read branch")?;

    // check if the branch upstream can be merged into the wd cleanly
    let repo = &project_repository.git_repository;

    // get upstream from the branch and find the remote branch
    let mut upstream_commit = None;
    let upstream_branch = branch
        .upstream
        .as_ref()
        .context("no upstream branch found")?;
    if let Ok(upstream_oid) = repo.refname_to_id(&upstream_branch.to_string()) {
        if let Ok(upstream_commit_obj) = repo.find_commit(upstream_oid) {
            upstream_commit = Some(upstream_commit_obj);
        }
    }

    // if there is no upstream commit, then there is nothing to do
    if upstream_commit.is_none() {
        // no upstream commit, no merge to be done
        return Ok(());
    }

    // there is an upstream commit, so lets check it out
    let upstream_commit = upstream_commit.unwrap();
    let remote_tree = upstream_commit.tree()?;

    if upstream_commit.id() == branch.head {
        // upstream is already merged, nothing to do
        return Ok(());
    }

    // if any other branches are applied, unapply them
    let applied_branches = Iterator::new(&current_session_reader)
        .context("failed to create branch iterator")?
        .collect::<Result<Vec<branch::Branch>, reader::Error>>()
        .context("failed to read virtual branches")?
        .into_iter()
        .filter(|b| b.applied)
        .filter(|b| b.id != *branch_id)
        .collect::<Vec<_>>();

    // unapply all other branches
    for other_branch in applied_branches {
        unapply_branch(gb_repository, project_repository, &other_branch.id)
            .context("failed to unapply branch")?;
    }

    // get merge base from remote branch commit and target commit
    let merge_base = repo
        .merge_base(upstream_commit.id(), branch.head)
        .context("failed to find merge base")?;
    let merge_tree = repo.find_commit(merge_base)?.tree()?;

    // get wd tree
    let wd_tree = project_repository.get_wd_tree()?;

    // try to merge our wd tree with the upstream tree
    let mut merge_index = repo
        .merge_trees(&merge_tree, &wd_tree, &remote_tree)
        .context("failed to merge trees")?;

    if merge_index.has_conflicts() {
        // checkout the conflicts
        repo.checkout_index(&mut merge_index)
            .allow_conflicts()
            .conflict_style_merge()
            .force()
            .checkout()?;

        // mark conflicts
        let conflicts = merge_index.conflicts()?;
        let mut merge_conflicts = Vec::new();
        for path in conflicts.flatten() {
            if let Some(ours) = path.our {
                let path = std::str::from_utf8(&ours.path)?.to_string();
                merge_conflicts.push(path);
            }
        }
        conflicts::mark(
            project_repository,
            &merge_conflicts,
            Some(upstream_commit.id()),
        )?;
    } else {
        // get the merge tree oid from writing the index out
        let merge_tree_oid = merge_index
            .write_tree_to(repo)
            .context("failed to write tree")?;

        let (author, committer) = project_repository.git_signatures(user)?;
        let message = "merged from upstream";

        let head_commit = repo.find_commit(branch.head)?;
        let merge_tree = repo.find_tree(merge_tree_oid)?;

        let new_branch_head = if let Some(key) = signing_key {
            repo.commit_signed(
                &author,
                message,
                &merge_tree,
                &[&head_commit, &upstream_commit],
                key,
            )
        } else {
            repo.commit(
                None,
                &author,
                &committer,
                message,
                &merge_tree,
                &[&head_commit, &upstream_commit],
            )
        }?;

        // checkout the merge tree
        repo.checkout_tree(&merge_tree).force().checkout()?;

        // write the branch data
        let branch_writer = branch::Writer::new(gb_repository);
        branch.head = new_branch_head;
        branch.tree = merge_tree_oid;
        branch_writer.write(&branch)?;
    }

    super::integration::update_gitbutler_integration(gb_repository, project_repository)?;

    Ok(())
}

pub fn update_branch(
    gb_repository: &gb_repository::Repository,
    project_repository: &project_repository::Repository,
    branch_update: branch::BranchUpdateRequest,
) -> Result<branch::Branch> {
    let current_session = gb_repository
        .get_or_create_current_session()
        .context("failed to get or create currnt session")?;
    let current_session_reader = sessions::Reader::open(gb_repository, &current_session)
        .context("failed to open current session")?;
    let branch_reader = branch::Reader::new(&current_session_reader);
    let branch_writer = branch::Writer::new(gb_repository);

    let mut branch = branch_reader
        .read(&branch_update.id)
        .context("failed to read branch")?;

    if let Some(ownership) = branch_update.ownership {
        set_ownership(&branch_reader, &branch_writer, &mut branch, &ownership)
            .context("failed to set ownership")?;
    }

    if let Some(name) = branch_update.name {
        let all_virtual_branches = Iterator::new(&current_session_reader)
            .context("failed to create branch iterator")?
            .collect::<Result<Vec<branch::Branch>, reader::Error>>()
            .context("failed to read virtual branches")?;

        project_repository.delete_branch_reference(&branch)?;

        branch.name = dedup(
            &all_virtual_branches
                .iter()
                .map(|b| b.name.as_str())
                .collect::<Vec<_>>(),
            &name,
        );

        project_repository.add_branch_reference(&branch)?;
    };

    if let Some(upstream_branch_name) = branch_update.upstream {
        let remote_branch = match get_default_target(&current_session_reader)? {
            Some(target) => format!(
                "refs/remotes/{}/{}",
                target.branch.remote(),
                slugify(upstream_branch_name)
            )
            .parse::<git::RemoteBranchName>()
            .unwrap(),
            None => bail!("no remote set"),
        };
        branch.upstream = Some(remote_branch);
    };

    if let Some(notes) = branch_update.notes {
        branch.notes = notes;
    };

    if let Some(order) = branch_update.order {
        branch.order = order;
    };

    branch_writer
        .write(&branch)
        .context("failed to write target branch")?;

    Ok(branch)
}

pub fn delete_branch(
    gb_repository: &gb_repository::Repository,
    project_repository: &project_repository::Repository,
    branch_id: &BranchId,
) -> Result<branch::Branch> {
    let current_session = gb_repository
        .get_or_create_current_session()
        .context("failed to get or create currnt session")?;
    let current_session_reader = sessions::Reader::open(gb_repository, &current_session)
        .context("failed to open current session")?;
    let branch_reader = branch::Reader::new(&current_session_reader);
    let branch_writer = branch::Writer::new(gb_repository);

    let branch = branch_reader
        .read(branch_id)
        .context("failed to read branch")?;

    branch_writer
        .delete(&branch)
        .context("Failed to remove branch")?;

    project_repository.delete_branch_reference(&branch)?;
    Ok(branch)
}

fn set_ownership(
    branch_reader: &branch::Reader,
    branch_writer: &branch::Writer,
    target_branch: &mut branch::Branch,
    ownership: &branch::Ownership,
) -> Result<()> {
    if target_branch.ownership.eq(ownership) {
        // nothing to update
        return Ok(());
    }

    let mut virtual_branches = Iterator::new(branch_reader.reader())
        .context("failed to create branch iterator")?
        .collect::<Result<Vec<branch::Branch>, reader::Error>>()
        .context("failed to read virtual branches")?
        .into_iter()
        .filter(|branch| branch.applied)
        .filter(|branch| branch.id != target_branch.id)
        .collect::<Vec<_>>();

    for file_ownership in &ownership.files {
        for branch in &mut virtual_branches {
            let taken = branch.ownership.take(file_ownership);
            if !taken.is_empty() {
                branch_writer.write(branch).context(format!(
                    "failed to write source branch for {}",
                    file_ownership
                ))?;
            }
        }
    }

    target_branch.ownership = ownership.clone();

    Ok(())
}

fn get_mtime(cache: &mut HashMap<path::PathBuf, u128>, file_path: &path::PathBuf) -> u128 {
    if let Some(mtime) = cache.get(file_path) {
        *mtime
    } else {
        let mtime = file_path
            .metadata()
            .map_or_else(
                |_| time::SystemTime::now(),
                |metadata| {
                    metadata
                        .modified()
                        .or(metadata.created())
                        .unwrap_or_else(|_| time::SystemTime::now())
                },
            )
            .duration_since(time::UNIX_EPOCH)
            .unwrap()
            .as_millis();
        cache.insert(file_path.clone(), mtime);
        mtime
    }
}

fn diff_hash(diff: &str) -> String {
    let addition = diff
        .lines()
        .skip(1) // skip the first line which is the diff header
        .filter(|line| line.starts_with('+') || line.starts_with('-')) // exclude context lines
        .collect::<Vec<_>>()
        .join("\n");
    format!("{:x}", md5::compute(addition))
}

pub fn virtual_hunks_by_filepath(
    repository: &git::Repository,
    diff: &HashMap<path::PathBuf, Vec<diff::Hunk>>,
) -> HashMap<path::PathBuf, Vec<VirtualBranchHunk>> {
    let mut mtimes: HashMap<path::PathBuf, u128> = HashMap::new();
    diff.iter()
        .map(|(file_path, hunks)| {
            let hunks = hunks
                .iter()
                .map(|hunk| VirtualBranchHunk {
                    id: format!("{}-{}", hunk.new_start, hunk.new_start + hunk.new_lines),
                    modified_at: get_mtime(&mut mtimes, &repository.path().join(file_path)),
                    file_path: file_path.clone(),
                    diff: hunk.diff.clone(),
                    start: hunk.new_start,
                    end: hunk.new_start + hunk.new_lines,
                    binary: hunk.binary,
                    hash: diff_hash(&hunk.diff),
                    locked: false,
                })
                .collect::<Vec<_>>();
            (file_path.clone(), hunks)
        })
        .collect::<HashMap<_, _>>()
}

type BranchStatus = HashMap<path::PathBuf, Vec<diff::Hunk>>;

// list the virtual branches and their file statuses (statusi?)
pub fn get_status_by_branch(
    gb_repository: &gb_repository::Repository,
    project_repository: &project_repository::Repository,
) -> Result<Vec<(branch::Branch, BranchStatus)>> {
    let current_session = gb_repository
        .get_or_create_current_session()
        .context("failed to get or create currnt session")?;
    let current_session_reader = sessions::Reader::open(gb_repository, &current_session)
        .context("failed to open current session")?;

    let default_target = match get_default_target(&current_session_reader)
        .context("failed to read default target")?
    {
        Some(target) => target,
        None => {
            return Ok(vec![]);
        }
    };

    let virtual_branches = Iterator::new(&current_session_reader)
        .context("failed to create branch iterator")?
        .collect::<Result<Vec<branch::Branch>, reader::Error>>()
        .context("failed to read virtual branches")?;

    let applied_virtual_branches = virtual_branches
        .iter()
        .filter(|branch| branch.applied)
        .cloned()
        .collect::<Vec<_>>();

    let applied_status = get_applied_status(
        gb_repository,
        project_repository,
        &default_target,
        applied_virtual_branches,
    )?;

    let non_applied_virtual_branches = virtual_branches
        .into_iter()
        .filter(|branch| !branch.applied)
        .collect::<Vec<_>>();

    let non_applied_status = get_non_applied_status(
        project_repository,
        &default_target,
        non_applied_virtual_branches,
    )?;

    Ok(applied_status
        .into_iter()
        .chain(non_applied_status)
        .collect())
}

// given a list of non applied virtual branches, return the status of each file, comparing the default target with
// virtual branch latest tree
//
// ownerships are not taken into account here, as they are not relevant for non applied branches
fn get_non_applied_status(
    project_repository: &project_repository::Repository,
    default_target: &target::Target,
    virtual_branches: Vec<branch::Branch>,
) -> Result<Vec<(branch::Branch, BranchStatus)>> {
    virtual_branches
        .into_iter()
        .map(
            |branch| -> Result<(branch::Branch, HashMap<path::PathBuf, Vec<diff::Hunk>>)> {
                if branch.applied {
                    bail!("branch {} is applied", branch.name);
                }
                let branch_tree = project_repository
                    .git_repository
                    .find_tree(branch.tree)
                    .context(format!("failed to find tree {}", branch.tree))?;
                let target_tree = project_repository
                    .git_repository
                    .find_commit(default_target.sha)
                    .context("failed to find target commit")?
                    .tree()
                    .context("failed to find target tree")?;

                let diff = diff::trees(
                    &project_repository.git_repository,
                    &target_tree,
                    &branch_tree,
                )?;

                Ok((branch, diff))
            },
        )
        .collect::<Result<Vec<_>>>()
}

// given a list of applied virtual branches, return the status of each file, comparing the default target with
// the working directory
//
// ownerships are updated if nessessary
fn get_applied_status(
    gb_repository: &gb_repository::Repository,
    project_repository: &project_repository::Repository,
    default_target: &target::Target,
    mut virtual_branches: Vec<branch::Branch>,
) -> Result<AppliedStatuses> {
    let mut diff = diff::workdir(
        &project_repository.git_repository,
        &default_target.sha,
        &diff::Options::default(),
    )
    .context("failed to diff")?;

    // sort by order, so that the default branch is first (left in the ui)
    virtual_branches.sort_by(|a, b| a.order.cmp(&b.order));

    if virtual_branches.is_empty() && !diff.is_empty() {
        // no virtual branches, but hunks: create default branch
        virtual_branches = vec![create_virtual_branch(
            gb_repository,
            project_repository,
            &BranchCreateRequest::default(),
        )
        .context("failed to default branch")?];
    }

    // align branch ownership to the real hunks:
    // - update shifted hunks
    // - remove non existent hunks

    let mut hunks_by_branch_id: HashMap<BranchId, HashMap<path::PathBuf, Vec<diff::Hunk>>> =
        virtual_branches
            .iter()
            .map(|branch| (branch.id, HashMap::new()))
            .collect();

    let mut mtimes = HashMap::new();

    for branch in &mut virtual_branches {
        if !branch.applied {
            bail!("branch {} is not applied", branch.name);
        }

        let mut updated: Vec<_> = vec![];
        branch.ownership = Ownership {
            files: branch
                .ownership
                .files
                .iter()
                .filter_map(|file_owership| {
                    let current_hunks = match diff.get_mut(&file_owership.file_path) {
                        None => {
                            // if the file is not in the diff, we don't want it
                            return None;
                        }
                        Some(hunks) => hunks,
                    };

                    let mtime = get_mtime(&mut mtimes, &file_owership.file_path);

                    let updated_hunks: Vec<Hunk> = file_owership
                        .hunks
                        .iter()
                        .filter_map(|owned_hunk| {
                            // if any of the current hunks intersects with the owned hunk, we want to keep it
                            for (i, ch) in current_hunks.iter().enumerate() {
                                let current_hunk = Hunk::from(ch);
                                if owned_hunk.eq(&current_hunk) {
                                    // try to re-use old timestamp
                                    let timestamp = owned_hunk.timestam_ms().unwrap_or(mtime);

                                    // push hunk to the end of the list, preserving the order
                                    hunks_by_branch_id
                                        .entry(branch.id)
                                        .or_default()
                                        .entry(file_owership.file_path.clone())
                                        .or_default()
                                        .push(ch.clone());

                                    // remove the hunk from the current hunks because each hunk can
                                    // only be owned once
                                    current_hunks.remove(i);

                                    return Some(owned_hunk.with_timestamp(timestamp));
                                } else if owned_hunk.intersects(&current_hunk) {
                                    // if it's an intersection, push the hunk to the beginning,
                                    // indicating the the hunk has been updated
                                    hunks_by_branch_id
                                        .entry(branch.id)
                                        .or_default()
                                        .entry(file_owership.file_path.clone())
                                        .or_default()
                                        .insert(0, ch.clone());

                                    // track updated hunks to bubble them up later
                                    updated.push(FileOwnership {
                                        file_path: file_owership.file_path.clone(),
                                        hunks: vec![current_hunk.clone()],
                                    });

                                    // remove the hunk from the current hunks because each hunk can
                                    // only be owned once
                                    current_hunks.remove(i);

                                    // return updated version, with new hash and/or timestamp
                                    return Some(current_hunk);
                                }
                            }
                            None
                        })
                        .collect();

                    if updated_hunks.is_empty() {
                        // if there are no hunks left, we don't want the file either
                        None
                    } else {
                        Some(FileOwnership {
                            file_path: file_owership.file_path.clone(),
                            hunks: updated_hunks,
                        })
                    }
                })
                .collect(),
        };

        // add the updated hunks to the branch again to promote them to the top
        updated
            .iter()
            .for_each(|file_ownership| branch.ownership.put(file_ownership));
    }

    // put the remaining hunks into the default (first) branch
    for (filepath, hunks) in diff {
        for hunk in hunks {
            virtual_branches[0].ownership.put(&FileOwnership {
                file_path: filepath.clone(),
                hunks: vec![Hunk::from(&hunk)
                    .with_timestamp(get_mtime(&mut mtimes, &filepath))
                    .with_hash(diff_hash(hunk.diff.as_str()).as_str())],
            });
            hunks_by_branch_id
                .entry(virtual_branches[0].id)
                .or_default()
                .entry(filepath.clone())
                .or_default()
                .push(hunk.clone());
        }
    }

    // write updated state
    let branch_writer = branch::Writer::new(gb_repository);
    for vranch in &virtual_branches {
        branch_writer
            .write(vranch)
            .context(format!("failed to write virtual branch {}", vranch.name))?;
    }

    let hunks_by_branch = hunks_by_branch_id
        .into_iter()
        .map(|(branch_id, hunks)| {
            (
                virtual_branches
                    .iter()
                    .find(|b| b.id.eq(&branch_id))
                    .unwrap()
                    .clone(),
                hunks,
            )
        })
        .collect::<Vec<_>>();

    Ok(hunks_by_branch)
}

fn virtual_hunks_to_virtual_files(
    project_repository: &project_repository::Repository,
    hunks: &[VirtualBranchHunk],
) -> Vec<VirtualBranchFile> {
    hunks
        .iter()
        .fold(HashMap::<path::PathBuf, Vec<_>>::new(), |mut acc, hunk| {
            acc.entry(hunk.file_path.clone())
                .or_default()
                .push(hunk.clone());
            acc
        })
        .into_iter()
        .map(|(file_path, hunks)| VirtualBranchFile {
            id: file_path.display().to_string(),
            path: file_path.clone(),
            hunks: hunks.clone(),
            binary: hunks.iter().any(|h| h.binary),
            modified_at: hunks.iter().map(|h| h.modified_at).max().unwrap_or(0),
            conflicted: conflicts::is_conflicting(
                project_repository,
                Some(&file_path.display().to_string()),
            )
            .unwrap_or(false),
        })
        .collect::<Vec<_>>()
}

// reset virtual branch to a specific commit
pub fn reset_branch(
    gb_repository: &gb_repository::Repository,
    project_repository: &project_repository::Repository,
    branch_id: &BranchId,
    target_commit_oid: git::Oid,
) -> Result<()> {
    let current_session = gb_repository.get_or_create_current_session()?;
    let current_session_reader = sessions::Reader::open(gb_repository, &current_session)?;

    let default_target =
        get_default_target(&current_session_reader)?.context("no default target")?;

    let branch_reader = branch::Reader::new(&current_session_reader);
    let branch = branch_reader.read(branch_id)?;

    if branch.head == target_commit_oid {
        // nothing to do
        return Ok(());
    }

    if default_target.sha != target_commit_oid
        && !project_repository
            .l(branch.head, LogUntil::Commit(default_target.sha))?
            .contains(&target_commit_oid)
    {
        bail!(
            "commit {} is not in branch {}",
            target_commit_oid,
            branch.name
        );
    }

    let branch_writer = branch::Writer::new(gb_repository);
    branch_writer
        .write(&branch::Branch {
            head: target_commit_oid,
            ..branch
        })
        .context("failed to write branch")?;

    super::integration::update_gitbutler_integration(gb_repository, project_repository)
        .context("failed to update gitbutler integration")?;

    Ok(())
}

fn diffs_to_virtual_files(
    project_repository: &project_repository::Repository,
    diffs: &HashMap<path::PathBuf, Vec<diff::Hunk>>,
) -> Vec<VirtualBranchFile> {
    let hunks_by_filepath = virtual_hunks_by_filepath(&project_repository.git_repository, diffs);
    virtual_hunks_to_virtual_files(
        project_repository,
        &hunks_by_filepath
            .values()
            .flatten()
            .cloned()
            .collect::<Vec<_>>(),
    )
}

// this function takes a list of file ownership,
// constructs a tree from those changes on top of the target
// and writes it as a new tree for storage
pub fn write_tree(
    project_repository: &project_repository::Repository,
    target: &target::Target,
    files: &HashMap<path::PathBuf, Vec<diff::Hunk>>,
) -> Result<git::Oid> {
    write_tree_onto_commit(project_repository, target.sha, files)
}

fn write_tree_onto_commit(
    project_repository: &project_repository::Repository,
    commit_oid: git::Oid,
    files: &HashMap<path::PathBuf, Vec<diff::Hunk>>,
) -> Result<git::Oid> {
    // read the base sha into an index
    let git_repository = &project_repository.git_repository;

    let head_commit = git_repository.find_commit(commit_oid)?;
    let base_tree = head_commit.tree()?;

    let mut builder = git_repository.treebuilder(Some(&base_tree));
    // now update the index with content in the working directory for each file
    for (filepath, hunks) in files {
        // convert this string to a Path
        let rel_path = std::path::Path::new(&filepath);
        let full_path = project_repository.path().join(rel_path);

        // if file exists
        if full_path.exists() {
            // if file is executable, use 755, otherwise 644
            let mut filemode = git::FileMode::Blob;
            // check if full_path file is executable
            if let Ok(metadata) = std::fs::symlink_metadata(&full_path) {
                if metadata.permissions().mode() & 0o111 != 0 {
                    filemode = git::FileMode::BlobExecutable;
                }
                if metadata.file_type().is_symlink() {
                    filemode = git::FileMode::Link;
                }
            }

            // get the blob
            if filemode == git::FileMode::Link {
                // it's a symlink, make the content the path of the link
                let link_target = std::fs::read_link(&full_path)?;

                // if the link target is inside the project repository, make it relative
                let link_target = link_target
                    .strip_prefix(project_repository.path())
                    .unwrap_or(&link_target);

                let blob_oid = git_repository.blob(link_target.as_os_str().as_bytes())?;
                builder.upsert(rel_path, blob_oid, filemode);
            } else if let Ok(tree_entry) = base_tree.get_path(rel_path) {
                if hunks.len() == 1 && hunks[0].binary {
                    let new_blob_oid = &hunks[0].diff;
                    // convert string to Oid
                    let new_blob_oid = new_blob_oid.parse().context("failed to diff as oid")?;
                    builder.upsert(rel_path, new_blob_oid, filemode);
                } else {
                    // blob from tree_entry
                    let blob = tree_entry
                        .to_object(git_repository)
                        .unwrap()
                        .peel_to_blob()
                        .context("failed to get blob")?;

                    // get the contents
                    let mut blob_contents = blob.content().to_vec();

                    let mut hunks = hunks.clone();
                    hunks.sort_by_key(|hunk| hunk.new_start);
                    for hunk in hunks {
                        let patch = format!("--- original\n+++ modified\n{}", hunk.diff);
                        let patch_bytes = patch.as_bytes();
                        let patch = Patch::from_bytes(patch_bytes)?;
                        blob_contents = apply_bytes(&blob_contents, &patch)
                            .context(format!("failed to apply {}", &hunk.diff))?;
                    }

                    // create a blob
                    let new_blob_oid = git_repository.blob(&blob_contents)?;
                    // upsert into the builder
                    builder.upsert(rel_path, new_blob_oid, filemode);
                }
            } else {
                // create a git blob from a file on disk
                let blob_oid = git_repository.blob_path(&full_path)?;
                builder.upsert(rel_path, blob_oid, filemode);
            }
        } else if base_tree.get_path(rel_path).is_ok() {
            // remove file from index if it exists in the base tree
            builder.remove(rel_path);
        } else {
            // file not in index or base tree, do nothing
            // this is the
        }
    }

    // now write out the tree
    let tree_oid = builder.write().context("failed to write updated tree")?;

    Ok(tree_oid)
}

fn _print_tree(repo: &git2::Repository, tree: &git2::Tree) -> Result<()> {
    println!("tree id: {}", tree.id());
    for entry in tree {
        println!(
            "  entry: {} {}",
            entry.name().unwrap_or_default(),
            entry.id()
        );
        // get entry contents
        let object = entry.to_object(repo).context("failed to get object")?;
        let blob = object.as_blob().context("failed to get blob")?;
        // convert content to string
        if let Ok(content) = std::str::from_utf8(blob.content()) {
            println!("    blob: {}", content);
        } else {
            println!("    blob: BINARY");
        }
    }
    Ok(())
}

pub fn commit(
    gb_repository: &gb_repository::Repository,
    project_repository: &project_repository::Repository,
    branch_id: &BranchId,
    message: &str,
    ownership: Option<&branch::Ownership>,
    signing_key: Option<&keys::PrivateKey>,
    user: Option<&users::User>,
) -> Result<git::Oid, CommitError> {
    let default_target = gb_repository
        .default_target()
        .context("failed to get default target")?
        .context("no default target set")?;

    // get the files to commit
    let statuses = get_status_by_branch(gb_repository, project_repository)
        .context("failed to get status by branch")?;

    let (branch, files) = statuses
        .iter()
        .find(|(branch, _)| branch.id == *branch_id)
        .ok_or_else(|| anyhow!("branch {} not found", branch_id))?;

    let files = calculate_non_commited_diffs(project_repository, branch, &default_target, files)?;
    if conflicts::is_conflicting(project_repository, None)? {
        return Err(CommitError::Conflicted);
    }

    let tree_oid = if let Some(ownership) = ownership {
        let files = files
            .iter()
            .filter_map(|(filepath, hunks)| {
                let hunks = hunks
                    .iter()
                    .filter(|hunk| {
                        ownership
                            .files
                            .iter()
                            .find(|f| f.file_path.eq(filepath))
                            .map_or(false, |f| {
                                f.hunks.iter().any(|h| {
                                    h.start == hunk.new_start
                                        && h.end == hunk.new_start + hunk.new_lines
                                })
                            })
                    })
                    .cloned()
                    .collect::<Vec<_>>();
                if hunks.is_empty() {
                    None
                } else {
                    Some((filepath.clone(), hunks))
                }
            })
            .collect::<HashMap<_, _>>();
        write_tree_onto_commit(project_repository, branch.head, &files)?
    } else {
        write_tree_onto_commit(project_repository, branch.head, &files)?
    };

    let git_repository = &project_repository.git_repository;
    let parent_commit = git_repository
        .find_commit(branch.head)
        .context(format!("failed to find commit {:?}", branch.head))?;
    let tree = git_repository
        .find_tree(tree_oid)
        .context(format!("failed to find tree {:?}", tree_oid))?;

    // now write a commit, using a merge parent if it exists
    let (author, committer) = project_repository.git_signatures(user)?;
    let extra_merge_parent =
        conflicts::merge_parent(project_repository).context("failed to get merge parent")?;

    let commit_oid = match extra_merge_parent {
        Some(merge_parent) => {
            let merge_parent = git_repository
                .find_commit(merge_parent)
                .context(format!("failed to find merge parent {:?}", merge_parent))?;
            let commit_oid = if let Some(key) = signing_key {
                git_repository.commit_signed(
                    &author,
                    message,
                    &tree,
                    &[&parent_commit, &merge_parent],
                    key,
                )
            } else {
                git_repository.commit(
                    None,
                    &author,
                    &committer,
                    message,
                    &tree,
                    &[&parent_commit, &merge_parent],
                )
            }
            .context("failed to commit")?;
            conflicts::clear(project_repository).context("failed to clear conflicts")?;
            commit_oid
        }
        None => if let Some(key) = signing_key {
            git_repository.commit_signed(&author, message, &tree, &[&parent_commit], key)
        } else {
            git_repository.commit(None, &author, &committer, message, &tree, &[&parent_commit])
        }
        .context("failed to commit")?,
    };

    // update the virtual branch head
    let writer = branch::Writer::new(gb_repository);
    writer
        .write(&Branch {
            tree: tree_oid,
            head: commit_oid,
            ..branch.clone()
        })
        .context("failed to write branch")?;

    super::integration::update_gitbutler_integration(gb_repository, project_repository)
        .context("failed to update gitbutler integration")?;

    Ok(commit_oid)
}

#[derive(Debug, thiserror::Error)]
pub enum CommitError {
    #[error("will not commit conflicted files")]
    Conflicted,
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

#[derive(Debug, thiserror::Error)]
pub enum PushError {
    #[error(transparent)]
    Remote(#[from] project_repository::RemoteError),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

pub fn push(
    project_repository: &project_repository::Repository,
    gb_repository: &gb_repository::Repository,
    branch_id: &BranchId,
    with_force: bool,
    credentials: &git::credentials::Factory,
) -> Result<(), PushError> {
    let current_session = gb_repository
        .get_or_create_current_session()
        .context("failed to get or create currnt session")
        .map_err(PushError::Other)?;
    let current_session_reader = sessions::Reader::open(gb_repository, &current_session)
        .context("failed to open current session")
        .map_err(PushError::Other)?;

    let branch_reader = branch::Reader::new(&current_session_reader);
    let branch_writer = branch::Writer::new(gb_repository);

    let vbranch = branch_reader
        .read(branch_id)
        .context("failed to read branch")
        .map_err(PushError::Other)?;

    let remote_branch = if let Some(upstream_branch) = vbranch.upstream.as_ref() {
        upstream_branch.clone()
    } else {
        let remote_branch = match get_default_target(&current_session_reader)? {
            Some(target) => format!(
                "refs/remotes/{}/{}",
                target.branch.remote(),
                slugify(&vbranch.name)
            )
            .parse::<git::RemoteBranchName>()
            .unwrap(),
            None => return Err(PushError::Other(anyhow::anyhow!("no default target set"))),
        };

        let remote_branches = project_repository.git_remote_branches()?;
        let existing_branches = remote_branches
            .iter()
            .map(RemoteBranchName::branch)
            .map(str::to_lowercase) // git is weird about case sensitivity here, assume not case sensitive
            .collect::<Vec<_>>();

        remote_branch.with_branch(&dedup_fmt(
            &existing_branches
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            remote_branch.branch(),
            "-",
        ))
    };

    project_repository.push(&vbranch.head, &remote_branch, with_force, credentials)?;

    branch_writer
        .write(&branch::Branch {
            upstream: Some(remote_branch.clone()),
            upstream_head: Some(vbranch.head),
            ..vbranch
        })
        .context("failed to write target branch after push")?;

    project_repository.fetch(remote_branch.remote(), credentials)?;

    Ok(())
}

pub fn mark_all_unapplied(gb_repository: &gb_repository::Repository) -> Result<()> {
    let current_session = gb_repository.get_or_create_current_session()?;
    let session_reader = sessions::Reader::open(gb_repository, &current_session)?;
    let branch_iterator = super::Iterator::new(&session_reader)?;
    let branch_writer = super::branch::Writer::new(gb_repository);
    branch_iterator
        .collect::<Result<Vec<_>, _>>()
        .context("failed to read branches")?
        .into_iter()
        .filter(|branch| branch.applied)
        .map(|branch| {
            branch_writer.write(&super::Branch {
                applied: false,
                ..branch
            })
        })
        .collect::<Result<Vec<_>, _>>()
        .context("failed to write branches")?;
    Ok(())
}

fn is_commit_integrated(
    project_repository: &project_repository::Repository,
    target: &target::Target,
    commit: &git::Commit,
) -> Result<bool> {
    let remote_branch = project_repository
        .git_repository
        .find_branch(&target.branch.clone().into())?;
    let remote_head = remote_branch.peel_to_commit()?;
    let upstream_commits = project_repository.l(
        remote_head.id(),
        project_repository::LogUntil::Commit(target.sha),
    )?;

    if target.sha.eq(&commit.id()) {
        // could not be integrated if heads are the same.
        return Ok(false);
    }

    if upstream_commits.is_empty() {
        // could not be integrated - there is nothing new upstream.
        return Ok(false);
    }

    if upstream_commits.contains(&commit.id()) {
        return Ok(true);
    }

    let merge_base = project_repository
        .git_repository
        .merge_base(target.sha, commit.id())?;
    if merge_base.eq(&commit.id()) {
        // if merge branch is the same as branch head and there are upstream commits
        // then it's integrated
        return Ok(true);
    }

    let merge_commit = project_repository.git_repository.find_commit(merge_base)?;
    let merge_tree = merge_commit.tree()?;
    let upstream = project_repository
        .git_repository
        .find_commit(remote_head.id())?;
    let upstream_tree = upstream.tree()?;
    let upstream_tree_oid = upstream_tree.id();

    // try to merge our tree into the upstream tree
    let mut merge_index = project_repository
        .git_repository
        .merge_trees(&merge_tree, &upstream_tree, &commit.tree()?)
        .context("failed to merge trees")?;

    if merge_index.has_conflicts() {
        return Ok(false);
    }

    let merge_tree_oid = merge_index
        .write_tree_to(&project_repository.git_repository)
        .context("failed to write tree")?;

    // if the merge_tree is the same as the new_target_tree and there are no files (uncommitted changes)
    // then the vbranch is fully merged
    Ok(merge_tree_oid == upstream_tree_oid)
}

pub fn is_remote_branch_mergeable(
    gb_repository: &gb_repository::Repository,
    project_repository: &project_repository::Repository,
    branch_name: &git::BranchName,
) -> Result<bool> {
    // get the current target
    let current_session = gb_repository
        .get_or_create_current_session()
        .context("failed to get or create currnt session")?;
    let current_session_reader = sessions::Reader::open(gb_repository, &current_session)
        .context("failed to open current session")?;

    let default_target =
        get_default_target(&current_session_reader)?.context("no default target set")?;

    let target_commit = project_repository
        .git_repository
        .find_commit(default_target.sha)
        .context("failed to find target commit")?;

    let branch = project_repository.git_repository.find_branch(branch_name)?;
    let branch_oid = branch.target().context("detatched head")?;
    let branch_commit = project_repository
        .git_repository
        .find_commit(branch_oid)
        .context("failed to find branch commit")?;

    let base_tree = find_base_tree(
        &project_repository.git_repository,
        &branch_commit,
        &target_commit,
    )?;

    let wd_tree = project_repository.get_wd_tree()?;

    let branch_tree = branch_commit.tree()?;
    let mergeable = !project_repository
        .git_repository
        .merge_trees(&base_tree, &branch_tree, &wd_tree)?
        .has_conflicts();

    Ok(mergeable)
}

pub fn is_virtual_branch_mergeable(
    gb_repository: &gb_repository::Repository,
    project_repository: &project_repository::Repository,
    branch_id: &BranchId,
) -> Result<bool> {
    let current_session = gb_repository
        .get_or_create_current_session()
        .context("failed to get or create currnt session")?;
    let current_session_reader = sessions::Reader::open(gb_repository, &current_session)
        .context("failed to open current session reader")?;
    let branch_reader = branch::Reader::new(&current_session_reader);
    let branch = branch_reader
        .read(branch_id)
        .context("failed to read branch")?;

    if branch.applied {
        return Ok(true);
    }

    let default_target = get_default_target(&current_session_reader)
        .context("failed to read default target")?
        .context("no default target set")?;

    // determine if this branch is up to date with the target/base
    let merge_base = project_repository
        .git_repository
        .merge_base(default_target.sha, branch.head)?;

    if merge_base != default_target.sha {
        return Ok(false);
    }

    let branch_commit = project_repository
        .git_repository
        .find_commit(branch.head)
        .context("failed to find branch commit")?;

    let target_commit = project_repository
        .git_repository
        .find_commit(default_target.sha)
        .context("failed to find target commit")?;

    let base_tree = find_base_tree(
        &project_repository.git_repository,
        &branch_commit,
        &target_commit,
    )?;

    let wd_tree = project_repository.get_wd_tree()?;

    // determine if this tree is mergeable
    let branch_tree = project_repository
        .git_repository
        .find_tree(branch.tree)
        .context("failed to find branch tree")?;

    let is_mergeable = !project_repository
        .git_repository
        .merge_trees(&base_tree, &branch_tree, &wd_tree)?
        .has_conflicts();

    Ok(is_mergeable)
}

pub fn amend(
    gb_repository: &gb_repository::Repository,
    project_repository: &project_repository::Repository,
    branch_id: &BranchId,
    target_ownership: &Ownership,
    signing_key: Option<&keys::PrivateKey>,
) -> Result<git::Oid> {
    if conflicts::is_conflicting(project_repository, None)? {
        bail!("cannot amend while conflicted");
    }

    let current_session = gb_repository
        .get_or_create_current_session()
        .context("failed to get or create current session")?;
    let current_session_reader = sessions::Reader::open(gb_repository, &current_session)
        .context("failed to open current session")?;

    let all_branches = Iterator::new(&current_session_reader)
        .context("failed to create branch iterator")?
        .collect::<Result<Vec<branch::Branch>, reader::Error>>()
        .context("failed to read virtual branches")?
        .into_iter()
        .collect::<Vec<_>>();

    if !all_branches.iter().any(|b| b.id == *branch_id) {
        bail!("branch not found");
    }

    let applied_branches = all_branches
        .into_iter()
        .filter(|b| b.applied)
        .collect::<Vec<_>>();

    if !applied_branches.iter().any(|b| b.id == *branch_id) {
        bail!("branch not applied");
    }

    let default_target = get_default_target(&current_session_reader)
        .context("failed to read default target")?
        .context("no default target set")?;

    let applied_statuses = get_applied_status(
        gb_repository,
        project_repository,
        &default_target,
        applied_branches,
    )?;

    let (target_branch, target_status) = applied_statuses
        .iter()
        .find(|(b, _)| b.id == *branch_id)
        .context("branch not found")?;

    if !target_branch.ownership.contains(target_ownership) {
        bail!("ownership not found");
    }

    if project_repository
        .l(
            target_branch.head,
            project_repository::LogUntil::Commit(default_target.sha),
        )?
        .is_empty()
    {
        bail!("branch doesn't have any commits");
    }

    let diffs_to_consider = calculate_non_commited_diffs(
        project_repository,
        target_branch,
        &default_target,
        target_status,
    )?;

    let head_commit = project_repository
        .git_repository
        .find_commit(target_branch.head)
        .context("failed to find head commit")?;

    let diffs_to_amend = target_ownership
        .files
        .iter()
        .filter_map(|file_ownership| {
            let hunks = diffs_to_consider
                .get(&file_ownership.file_path)
                .map(|hunks| {
                    hunks
                        .iter()
                        .filter(|hunk| {
                            file_ownership.hunks.iter().any(|owned_hunk| {
                                owned_hunk.start == hunk.new_start
                                    && owned_hunk.end == hunk.new_start + hunk.new_lines
                            })
                        })
                        .cloned()
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            if hunks.is_empty() {
                None
            } else {
                Some((file_ownership.file_path.clone(), hunks))
            }
        })
        .collect::<HashMap<_, _>>();

    let new_tree_oid =
        write_tree_onto_commit(project_repository, target_branch.head, &diffs_to_amend)?;
    let new_tree = project_repository
        .git_repository
        .find_tree(new_tree_oid)
        .context("failed to find new tree")?;

    let parents = head_commit
        .parents()
        .context("failed to find head commit parents")?;

    let commit_oid = if let Some(key) = signing_key {
        project_repository.git_repository.commit_signed(
            &head_commit.author(),
            head_commit.message().unwrap_or_default(),
            &new_tree,
            &parents.iter().collect::<Vec<_>>(),
            key,
        )
    } else {
        project_repository.git_repository.commit(
            None,
            &head_commit.author(),
            &head_commit.committer(),
            head_commit.message().unwrap_or_default(),
            &new_tree,
            &parents.iter().collect::<Vec<_>>(),
        )
    }
    .context("failed to create commit")?;

    let branch_writer = branch::Writer::new(gb_repository);
    branch_writer.write(&branch::Branch {
        head: commit_oid,
        ..target_branch.clone()
    })?;

    super::integration::update_gitbutler_integration(gb_repository, project_repository)?;

    Ok(commit_oid)
}

pub fn cherry_pick(
    gb_repository: &gb_repository::Repository,
    project_repository: &project_repository::Repository,
    branch_id: &BranchId,
    target_commit_oid: git::Oid,
) -> Result<Option<git::Oid>> {
    if conflicts::is_conflicting(project_repository, None)? {
        bail!("cannot cherry pick while conflicted");
    }

    let current_session = gb_repository
        .get_or_create_current_session()
        .context("failed to get or create current session")?;
    let current_session_reader = sessions::Reader::open(gb_repository, &current_session)
        .context("failed to open current session")?;
    let branch_reader = branch::Reader::new(&current_session_reader);
    let branch = branch_reader
        .read(branch_id)
        .context("failed to read branch")?;

    if !branch.applied {
        // todo?
        bail!("cannot cherry pick unapplied branch");
    }

    let target_commit = project_repository
        .git_repository
        .find_commit(target_commit_oid)
        .context("failed to find target commit")?;

    let branch_head_commit = project_repository
        .git_repository
        .find_commit(branch.head)
        .context("failed to find branch tree")?;

    let default_target = get_default_target(&current_session_reader)
        .context("failed to read default target")?
        .context("no default target set")?;

    // if any other branches are applied, unapply them
    let applied_branches = Iterator::new(&current_session_reader)
        .context("failed to create branch iterator")?
        .collect::<Result<Vec<branch::Branch>, reader::Error>>()
        .context("failed to read virtual branches")?
        .into_iter()
        .filter(|b| b.applied)
        .collect::<Vec<_>>();

    let applied_statuses = get_applied_status(
        gb_repository,
        project_repository,
        &default_target,
        applied_branches,
    )?;

    let branch_files = applied_statuses
        .iter()
        .find(|(b, _)| b.id == *branch_id)
        .map(|(_, f)| f)
        .context("branch status not found")?;

    // create a wip commit. we'll use it to offload cherrypick conflicts calculation to libgit.
    let wip_commit = {
        let wip_tree_oid = write_tree(project_repository, &default_target, branch_files)?;
        let wip_tree = project_repository.git_repository.find_tree(wip_tree_oid)?;

        let oid = project_repository.git_repository.commit(
            None,
            &git::Signature::now("GitButler", "gitbutler@gitbutler.com")?,
            &git::Signature::now("GitButler", "gitbutler@gitbutler.com")?,
            "wip cherry picking commit",
            &wip_tree,
            &[&branch_head_commit],
        )?;
        project_repository.git_repository.find_commit(oid)?
    };

    let mut cherrypick_index = project_repository
        .git_repository
        .cherry_pick(&wip_commit, &target_commit)
        .context("failed to cherry pick")?;

    // unapply other branches
    for other_branch in applied_statuses
        .iter()
        .filter(|(b, _)| b.id != branch.id)
        .map(|(b, _)| b)
    {
        unapply_branch(gb_repository, project_repository, &other_branch.id)
            .context("failed to unapply branch")?;
    }

    let commit_oid = if cherrypick_index.has_conflicts() {
        // checkout the conflicts
        project_repository
            .git_repository
            .checkout_index(&mut cherrypick_index)
            .allow_conflicts()
            .conflict_style_merge()
            .force()
            .checkout()?;

        // mark conflicts
        let conflicts = cherrypick_index.conflicts()?;
        let mut merge_conflicts = Vec::new();
        for path in conflicts.flatten() {
            if let Some(ours) = path.our {
                let path = std::str::from_utf8(&ours.path)?.to_string();
                merge_conflicts.push(path);
            }
        }
        conflicts::mark(project_repository, &merge_conflicts, Some(branch.head))?;

        None
    } else {
        let merge_tree_oid = cherrypick_index
            .write_tree_to(&project_repository.git_repository)
            .context("failed to write merge tree")?;
        let merge_tree = project_repository
            .git_repository
            .find_tree(merge_tree_oid)
            .context("failed to find merge tree")?;

        let branch_head_commit = project_repository
            .git_repository
            .find_commit(branch.head)
            .context("failed to find branch head commit")?;

        let commit_oid = project_repository
            .git_repository
            .commit(
                None,
                &target_commit.author(),
                &target_commit.committer(),
                target_commit.message().unwrap_or_default(),
                &merge_tree,
                &[&branch_head_commit],
            )
            .context("failed to create commit")?;

        // checkout final_tree into the working directory
        project_repository
            .git_repository
            .checkout_tree(&merge_tree)
            .force()
            .remove_untracked()
            .checkout()?;

        // update branch status
        let writer = branch::Writer::new(gb_repository);
        writer
            .write(&Branch {
                head: commit_oid,
                ..branch.clone()
            })
            .context("failed to write branch")?;

        Some(commit_oid)
    };

    super::integration::update_gitbutler_integration(gb_repository, project_repository)
        .context("failed to update gitbutler integration")?;

    Ok(commit_oid)
}

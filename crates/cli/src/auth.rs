//! `brick login` / `logout` / `whoami` command handlers (sync feature only).
//!
//! These drive the proprietary `brick_sync::identity` flow: email one-time-code
//! login against Supabase, local session persistence, and display. Account
//! identity is part of the closed networked surface, so this module only exists
//! under the `sync` feature — the open-source build has no login commands.

use anyhow::{Context, Result};
use brick_sync::identity;
use dialoguer::{Input, Password};

/// Handles `brick login`: requests an email OTP, prompts for the code, verifies
/// it, and persists the resulting login session.
pub fn handle_login(email: Option<String>) -> Result<()> {
    let email = match email {
        Some(email) => email,
        None => Input::new()
            .with_prompt("Email")
            .interact_text()
            .context("failed to read email")?,
    };

    identity::request_email_otp(&email)?;
    println!("A one-time code was sent to {email}.");

    let code: String = Password::new()
        .with_prompt("Enter the code")
        .interact()
        .context("failed to read one-time code")?;

    let session = identity::verify_email_otp(&email, code.trim())?;
    match &session.email {
        Some(email) => println!("Logged in as {email}."),
        None => println!("Logged in (user {}).", session.user_id),
    }
    Ok(())
}

/// Handles `brick logout`: removes the local login session.
pub fn handle_logout() -> Result<()> {
    if identity::clear()? {
        println!("Logged out.");
    } else {
        println!("Not logged in.");
    }
    Ok(())
}

/// Handles `brick whoami`: prints the current account, refreshing the token if
/// it has expired.
pub fn handle_whoami() -> Result<()> {
    match identity::load()? {
        None => {
            println!("Not logged in. Run `brick login`.");
        }
        Some(session) => {
            let session = if session.is_expired() {
                identity::refresh().unwrap_or(session)
            } else {
                session
            };
            match &session.email {
                Some(email) => println!("Logged in as {email} (user {}).", session.user_id),
                None => println!("Logged in (user {}).", session.user_id),
            }
        }
    }
    Ok(())
}

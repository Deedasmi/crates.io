use chrono::NaiveDateTime;
use conduit::{Request, Response};
use conduit_cookie::RequestSession;
use conduit_router::RequestParams;
use diesel::expression::now;
use diesel::prelude::*;
use rand::{thread_rng, Rng};
use std::borrow::Cow;
use serde_json;

use app::RequestApp;
use db::RequestTransaction;
use krate::Follow;
use pagination::Paginate;
use schema::*;
use util::{bad_request, human, CargoResult, RequestUtils};
use version::EncodableVersion;
use {github, Version};
use owner::{CrateOwner, Owner, OwnerKind};
use krate::Crate;
use email;

pub use self::middleware::{AuthenticationSource, Middleware, RequestUser};

pub mod middleware;

/// The model representing a row in the `users` database table.
#[derive(Clone, Debug, PartialEq, Eq, Queryable, Identifiable, AsChangeset, Associations)]
pub struct User {
    pub id: i32,
    pub email: Option<String>,
    pub gh_access_token: String,
    pub gh_login: String,
    pub name: Option<String>,
    pub gh_avatar: Option<String>,
    pub gh_id: i32,
}

#[derive(Insertable, AsChangeset, Debug)]
#[table_name = "users"]
pub struct NewUser<'a> {
    pub gh_id: i32,
    pub gh_login: &'a str,
    pub email: Option<&'a str>,
    pub name: Option<&'a str>,
    pub gh_avatar: Option<&'a str>,
    pub gh_access_token: Cow<'a, str>,
}

#[derive(Debug, Queryable, AsChangeset, Identifiable, Associations)]
#[belongs_to(User)]
pub struct Email {
    pub id: i32,
    pub user_id: i32,
    pub email: String,
    pub verified: bool,
}

#[derive(Debug, Insertable, AsChangeset)]
#[table_name = "emails"]
pub struct NewEmail<'a> {
    pub user_id: i32,
    pub email: &'a str,
    pub verified: bool,
}

#[derive(Debug, Queryable, AsChangeset, Identifiable, Associations)]
#[belongs_to(Email)]
pub struct Token {
    pub id: i32,
    pub email_id: i32,
    pub token: String,
    pub created_at: NaiveDateTime,
}

#[derive(Debug, Insertable, AsChangeset)]
#[table_name = "tokens"]
pub struct NewToken {
    email_id: i32,
    token: String,
}

impl NewToken {
    fn generate(email_id: i32) -> Self {
        NewToken {
            token: generate_token(),
            email_id,
        }
    }

    fn regenerate(id: i32, conn: &PgConnection) -> QueryResult<String> {
        use diesel::insert;
        use diesel::expression::now;
        use diesel::pg::upsert::*;
        use schema::tokens::dsl::{created_at, email_id, token, tokens};

        let new_token = Self::generate(id);
        insert(&new_token.on_conflict(
            email_id,
            do_update().set((token.eq(&new_token.token), created_at.eq(now))),
        )).into(tokens)
            .returning(token)
            .get_result(conn)
    }
}

impl<'a> NewUser<'a> {
    pub fn new(
        gh_id: i32,
        gh_login: &'a str,
        email: Option<&'a str>,
        name: Option<&'a str>,
        gh_avatar: Option<&'a str>,
        gh_access_token: &'a str,
    ) -> Self {
        NewUser {
            gh_id: gh_id,
            gh_login: gh_login,
            email: email,
            name: name,
            gh_avatar: gh_avatar,
            gh_access_token: Cow::Borrowed(gh_access_token),
        }
    }

    /// Inserts the user into the database, or updates an existing one.
    pub fn create_or_update(&self, conn: &PgConnection) -> QueryResult<User> {
        use diesel::insert;
        use diesel::expression::dsl::sql;
        use diesel::types::Integer;
        use diesel::pg::upsert::*;
        use diesel::NotFound;

        let update_user = NewUser {
            email: None,
            gh_id: self.gh_id,
            gh_login: self.gh_login,
            name: self.name,
            gh_avatar: self.gh_avatar,
            gh_access_token: self.gh_access_token.clone(),
        };

        conn.transaction(|| {
            // We need the `WHERE gh_id > 0` condition here because `gh_id` set
            // to `-1` indicates that we were unable to find a GitHub ID for
            // the associated GitHub login at the time that we backfilled
            // GitHub IDs. Therefore, there are multiple records in production
            // that have a `gh_id` of `-1` so we need to exclude those when
            // considering uniqueness of `gh_id` values. The `> 0` condition isn't
            // necessary for most fields in the database to be used as a conflict
            // target :)
            let conflict_target = sql::<Integer>("(gh_id) WHERE gh_id > 0");
            let user = insert(&self.on_conflict(conflict_target, do_update().set(&update_user)))
                .into(users::table)
                .get_result::<User>(conn)?;

            // To send the user an account verification email...
            if let Some(user_email) = user.email.as_ref() {
                let new_email = NewEmail {
                    user_id: user.id,
                    email: user_email,
                    verified: false,
                };

                let email_id = insert(&new_email.on_conflict_do_nothing())
                    .into(emails::table)
                    .returning(emails::id)
                    .get_result(conn)
                    .optional()?;

                if let Some(id) = email_id {
                    let new_token = NewToken::generate(id);

                    insert(&new_token).into(tokens::table).execute(conn)?;

                    send_user_confirm_email(user_email, &user.gh_login, &new_token.token)
                        .map_err(|_| NotFound)?;
                }
            }

            Ok(user)
        })
    }
}

/// The serialization format for the `User` model.
/// Same as private user, except no email field
#[derive(Deserialize, Serialize, Debug)]
pub struct EncodablePublicUser {
    pub id: i32,
    pub login: String,
    pub name: Option<String>,
    pub avatar: Option<String>,
    pub url: Option<String>,
}

/// The serialization format for the `User` model.
/// Same as public user, except for addition of
/// email field
#[derive(Deserialize, Serialize, Debug)]
pub struct EncodablePrivateUser {
    pub id: i32,
    pub login: String,
    pub email: Option<String>,
    pub email_verified: bool,
    pub email_verification_sent: bool,
    pub name: Option<String>,
    pub avatar: Option<String>,
    pub url: Option<String>,
}

impl User {
    /// Queries the database for a user with a certain `api_token` value.
    pub fn find_by_api_token(conn: &PgConnection, token_: &str) -> CargoResult<User> {
        use diesel::update;
        use schema::api_tokens::dsl::{api_tokens, last_used_at, token, user_id};
        use schema::users::dsl::{id, users};
        let user_id_ = update(api_tokens.filter(token.eq(token_)))
            .set(last_used_at.eq(now.nullable()))
            .returning(user_id)
            .get_result::<i32>(conn)?;
        Ok(users.filter(id.eq(user_id_)).get_result(conn)?)
    }

    pub fn owning(krate: &Crate, conn: &PgConnection) -> CargoResult<Vec<Owner>> {
        let base_query = CrateOwner::belonging_to(krate).filter(crate_owners::deleted.eq(false));
        let users = base_query
            .inner_join(users::table)
            .select(users::all_columns)
            .filter(crate_owners::owner_kind.eq(OwnerKind::User as i32))
            .load(conn)?
            .into_iter()
            .map(Owner::User);

        Ok(users.collect())
    }

    /// Converts this `User` model into an `EncodablePrivateUser` for JSON serialization.
    pub fn encodable_private(
        self,
        email_verified: bool,
        email_verification_sent: bool,
    ) -> EncodablePrivateUser {
        let User {
            id,
            email,
            name,
            gh_login,
            gh_avatar,
            ..
        } = self;
        let url = format!("https://github.com/{}", gh_login);
        EncodablePrivateUser {
            id: id,
            email: email,
            email_verified,
            email_verification_sent,
            avatar: gh_avatar,
            login: gh_login,
            name: name,
            url: Some(url),
        }
    }

    /// Converts this`User` model into an `EncodablePublicUser` for JSON serialization.
    pub fn encodable_public(self) -> EncodablePublicUser {
        let User {
            id,
            name,
            gh_login,
            gh_avatar,
            ..
        } = self;
        let url = format!("https://github.com/{}", gh_login);
        EncodablePublicUser {
            id: id,
            avatar: gh_avatar,
            login: gh_login,
            name: name,
            url: Some(url),
        }
    }
}

/// Handles the `GET /authorize_url` route.
///
/// This route will return an authorization URL for the GitHub OAuth flow including the crates.io
/// `client_id` and a randomly generated `state` secret.
///
/// see https://developer.github.com/v3/oauth/#redirect-users-to-request-github-access
///
/// ## Response Body Example
///
/// ```json
/// {
///     "state": "b84a63c4ea3fcb4ac84",
///     "url": "https://github.com/login/oauth/authorize?client_id=...&state=...&scope=read%3Aorg"
/// }
/// ```
pub fn github_authorize(req: &mut Request) -> CargoResult<Response> {
    // Generate a random 16 char ASCII string
    let state: String = thread_rng().gen_ascii_chars().take(16).collect();
    req.session()
        .insert("github_oauth_state".to_string(), state.clone());

    let url = req.app().github.authorize_url(state.clone());

    #[derive(Serialize)]
    struct R {
        url: String,
        state: String,
    }
    Ok(req.json(&R {
        url: url.to_string(),
        state: state,
    }))
}

/// Handles the `GET /authorize` route.
///
/// This route is called from the GitHub API OAuth flow after the user accepted or rejected
/// the data access permissions. It will check the `state` parameter and then call the GitHub API
/// to exchange the temporary `code` for an API token. The API token is returned together with
/// the corresponding user information.
///
/// see https://developer.github.com/v3/oauth/#github-redirects-back-to-your-site
///
/// ## Query Parameters
///
/// - `code` – temporary code received from the GitHub API  **(Required)**
/// - `state` – state parameter received from the GitHub API  **(Required)**
///
/// ## Response Body Example
///
/// ```json
/// {
///     "api_token": "b84a63c4ea3fcb4ac84",
///     "user": {
///         "email": "foo@bar.org",
///         "name": "Foo Bar",
///         "login": "foobar",
///         "avatar": "https://avatars.githubusercontent.com/u/1234",
///         "url": null
///     }
/// }
/// ```
pub fn github_access_token(req: &mut Request) -> CargoResult<Response> {
    // Parse the url query
    let mut query = req.query();
    let code = query.remove("code").unwrap_or_default();
    let state = query.remove("state").unwrap_or_default();

    // Make sure that the state we just got matches the session state that we
    // should have issued earlier.
    {
        let session_state = req.session().remove(&"github_oauth_state".to_string());
        let session_state = session_state.as_ref().map(|a| &a[..]);
        if Some(&state[..]) != session_state {
            return Err(human("invalid state parameter"));
        }
    }

    #[derive(Deserialize)]
    struct GithubUser {
        email: Option<String>,
        name: Option<String>,
        login: String,
        id: i32,
        avatar_url: Option<String>,
    }

    // Fetch the access token from github using the code we just got
    let token = req.app()
        .github
        .exchange(code.clone())
        .map_err(|s| human(&s))?;

    let (handle, resp) = github::github(req.app(), "/user", &token)?;
    let ghuser: GithubUser = github::parse_github_response(handle, &resp)?;

    let user = NewUser::new(
        ghuser.id,
        &ghuser.login,
        ghuser.email.as_ref().map(|s| &s[..]),
        ghuser.name.as_ref().map(|s| &s[..]),
        ghuser.avatar_url.as_ref().map(|s| &s[..]),
        &token.access_token,
    ).create_or_update(&*req.db_conn()?)?;
    req.session()
        .insert("user_id".to_string(), user.id.to_string());
    req.mut_extensions().insert(user);
    me(req)
}

/// Handles the `GET /logout` route.
pub fn logout(req: &mut Request) -> CargoResult<Response> {
    req.session().remove(&"user_id".to_string());
    Ok(req.json(&true))
}

/// Handles the `GET /me` route.
pub fn me(req: &mut Request) -> CargoResult<Response> {
    // Changed to getting User information from database because in
    // src/tests/user.rs, when testing put and get on updating email,
    // request seems to be somehow 'cached'. When we try to get a
    // request from the /me route with the just updated user (call
    // this function) the user is the same as the initial GET request
    // and does not seem to get the updated user information from the
    // database
    // This change is not preferable, we'd rather fix the request,
    // perhaps adding `req.mut_extensions().insert(user)` to the
    // update_user route, however this somehow does not seem to work

    use self::users::dsl::{id, users};
    use self::emails::dsl::{emails, user_id};
    use diesel::select;
    use diesel::expression::dsl::exists;

    let u_id = req.user()?.id;
    let conn = req.db_conn()?;

    let user_info = users.filter(id.eq(u_id)).first::<User>(&*conn)?;
    let email_result = emails.filter(user_id.eq(u_id)).first::<Email>(&*conn);

    let (email, verified, verification_sent): (Option<String>, bool, bool) = match email_result {
        Ok(email_record) => {
            let verification_sent = if email_record.verified {
                true
            } else {
                select(exists(Token::belonging_to(&email_record)))
                    .get_result(&*conn)
                    .unwrap_or(false)
            };
            (
                Some(email_record.email),
                email_record.verified,
                verification_sent,
            )
        }
        Err(_) => (None, false, false),
    };

    let user = User {
        email: email,
        ..user_info
    };

    #[derive(Serialize)]
    struct R {
        user: EncodablePrivateUser,
    }
    Ok(req.json(&R {
        user: user.encodable_private(verified, verification_sent),
    }))
}

/// Handles the `GET /users/:user_id` route.
pub fn show(req: &mut Request) -> CargoResult<Response> {
    use self::users::dsl::{gh_login, users};

    let name = &req.params()["user_id"].to_lowercase();
    let conn = req.db_conn()?;
    let user = users
        .filter(::lower(gh_login).eq(name))
        .first::<User>(&*conn)?;

    #[derive(Serialize)]
    struct R {
        user: EncodablePublicUser,
    }
    Ok(req.json(&R {
        user: user.encodable_public(),
    }))
}

/// Handles the `GET /teams/:team_id` route.
pub fn show_team(req: &mut Request) -> CargoResult<Response> {
    use self::teams::dsl::{login, teams};
    use owner::Team;
    use owner::EncodableTeam;

    let name = &req.params()["team_id"];
    let conn = req.db_conn()?;
    let team = teams.filter(login.eq(name)).first::<Team>(&*conn)?;

    #[derive(Serialize)]
    struct R {
        team: EncodableTeam,
    }
    Ok(req.json(&R {
        team: team.encodable(),
    }))
}

/// Handles the `GET /me/updates` route.
pub fn updates(req: &mut Request) -> CargoResult<Response> {
    use diesel::expression::dsl::any;

    let user = req.user()?;
    let (offset, limit) = req.pagination(10, 100)?;
    let conn = req.db_conn()?;

    let followed_crates = Follow::belonging_to(user).select(follows::crate_id);
    let data = versions::table
        .inner_join(crates::table)
        .filter(crates::id.eq(any(followed_crates)))
        .order(versions::created_at.desc())
        .select((versions::all_columns, crates::name))
        .paginate(limit, offset)
        .load::<((Version, String), i64)>(&*conn)?;

    let more = data.get(0)
        .map(|&(_, count)| count > offset + limit)
        .unwrap_or(false);

    let versions = data.into_iter()
        .map(|((version, crate_name), _)| version.encodable(&crate_name))
        .collect();

    #[derive(Serialize)]
    struct R {
        versions: Vec<EncodableVersion>,
        meta: Meta,
    }
    #[derive(Serialize)]
    struct Meta {
        more: bool,
    }
    Ok(req.json(&R {
        versions: versions,
        meta: Meta { more: more },
    }))
}

/// Handles the `GET /users/:user_id/stats` route.
pub fn stats(req: &mut Request) -> CargoResult<Response> {
    use diesel::expression::dsl::sum;
    use owner::OwnerKind;

    let user_id = &req.params()["user_id"].parse::<i32>().ok().unwrap();
    let conn = req.db_conn()?;

    let data = crate_owners::table
        .inner_join(crates::table)
        .filter(
            crate_owners::owner_id
                .eq(user_id)
                .and(crate_owners::owner_kind.eq(OwnerKind::User as i32)),
        )
        .select(sum(crates::downloads))
        .first::<Option<i64>>(&*conn)?
        .unwrap_or(0);

    #[derive(Serialize)]
    struct R {
        total_downloads: i64,
    }
    Ok(req.json(&R {
        total_downloads: data,
    }))
}

/// Handles the `PUT /user/:user_id` route.
pub fn update_user(req: &mut Request) -> CargoResult<Response> {
    use diesel::{insert, update};
    use diesel::pg::upsert::*;
    use self::emails::dsl::user_id;
    use self::users::dsl::{email, gh_login, users};

    let mut body = String::new();
    req.body().read_to_string(&mut body)?;
    let user = req.user()?;
    let name = &req.params()["user_id"];
    let conn = req.db_conn()?;

    // need to check if current user matches user to be updated
    if &user.id.to_string() != name {
        return Err(human("current user does not match requested user"));
    }

    #[derive(Deserialize)]
    struct UserUpdate {
        user: User,
    }

    #[derive(Deserialize)]
    struct User {
        email: Option<String>,
    }

    let user_update: UserUpdate =
        serde_json::from_str(&body).map_err(|_| human("invalid json request"))?;

    if user_update.user.email.is_none() {
        return Err(human("empty email rejected"));
    }

    let user_email = user_update.user.email.unwrap();
    let user_email = user_email.trim();

    if user_email == "" {
        return Err(human("empty email rejected"));
    }

    conn.transaction(|| {
        update(users.filter(gh_login.eq(&user.gh_login)))
            .set(email.eq(user_email))
            .execute(&*conn)?;

        let new_email = NewEmail {
            user_id: user.id,
            email: user_email,
            verified: false,
        };

        let email_id = insert(&new_email.on_conflict(user_id, do_update().set(&new_email)))
            .into(emails::table)
            .returning(emails::id)
            .get_result(&*conn)
            .map_err(|_| human("Error in updating email"))?;
        let token = NewToken::regenerate(email_id, &conn)?;

        send_user_confirm_email(user_email, &user.gh_login, &token)
            .map_err(|_| bad_request("Email could not be sent"))
    })?;

    #[derive(Serialize)]
    struct R {
        ok: bool,
    }
    Ok(req.json(&R { ok: true }))
}

fn send_user_confirm_email(email: &str, user_name: &str, token: &str) -> CargoResult<()> {
    // Create a URL with token string as path to send to user
    // If user clicks on path, look email/user up in database,
    // make sure tokens match

    let subject = "Please confirm your email address";
    let body = format!(
        "Hello {}! Welcome to Crates.io. Please click the
link below to verify your email address. Thank you!\n
https://crates.io/confirm/{}",
        user_name,
        token
    );

    email::send_email(email, subject, &body)
}

/// Handles the `PUT /confirm/:email_token` route
pub fn confirm_user_email(req: &mut Request) -> CargoResult<Response> {
    // to confirm, we must grab the token on the request as part of the URL
    // look up the token in the tokens table
    // find what user the token belongs to
    // on the email table, change 'verified' to true
    // delete the token from the tokens table
    use diesel::{delete, update};

    let conn = req.db_conn()?;
    let req_token = &req.params()["email_token"];

    let token_info = tokens::table
        .filter(tokens::token.eq(req_token))
        .first::<Token>(&*conn)
        .map_err(|_| bad_request("Email token not found."))?;

    let email_info = emails::table
        .filter(emails::id.eq(token_info.email_id))
        .first::<Email>(&*conn)
        .map_err(|_| bad_request("Email belonging to token not found."))?;

    update(emails::table.filter(emails::id.eq(email_info.id)))
        .set(emails::verified.eq(true))
        .execute(&*conn)
        .map_err(|_| bad_request("Email verification could not be updated"))?;

    delete(tokens::table.filter(tokens::id.eq(token_info.id)))
        .execute(&*conn)
        .map_err(|_| bad_request("Email token could not be deleted"))?;

    #[derive(Serialize)]
    struct R {
        ok: bool,
    }
    Ok(req.json(&R { ok: true }))
}

/// Handles `PUT /user/:user_id/resend` route
pub fn regenerate_token_and_send(req: &mut Request) -> CargoResult<Response> {
    let user = req.user()?;
    let name = &req.params()["user_id"].parse::<i32>().ok().unwrap();
    let conn = req.db_conn()?;

    // need to check if current user matches user to be updated
    if &user.id != name {
        return Err(human("current user does not match requested user"));
    }

    conn.transaction(|| {
        let email_info = emails::table
            .filter(emails::user_id.eq(user.id))
            .first::<Email>(&*conn)
            .map_err(|_| bad_request("Email could not be found"))?;

        let token = NewToken::regenerate(email_info.id, &conn)?;

        send_user_confirm_email(&email_info.email, &user.gh_login, &token)
            .map_err(|_| bad_request("Error in sending email"))
    })?;

    #[derive(Serialize)]
    struct R {
        ok: bool,
    }
    Ok(req.json(&R { ok: true }))
}

fn generate_token() -> String {
    let token: String = thread_rng().gen_ascii_chars().take(26).collect();
    token
}

use crate::structs::{PaginationCursor, PostView};
use diesel::{
  debug_query,
  dsl::{exists, not, IntervalDsl},
  pg::Pg,
  query_builder::AsQuery,
  result::Error,
  sql_types,
  BoolExpressionMethods,
  BoxableExpression,
  ExpressionMethods,
  IntoSql,
  JoinOnDsl,
  NullableExpressionMethods,
  OptionalExtension,
  PgTextExpressionMethods,
  QueryDsl,
};
use diesel_async::RunQueryDsl;
use i_love_jesus::PaginatedQueryBuilder;
use lemmy_db_schema::{
  aggregates::structs::{post_aggregates_keys as key, PostAggregates},
  impls::local_user::LocalUserOptionHelper,
  newtypes::{CommunityId, LocalUserId, PersonId, PostId},
  schema::{
    community,
    community_block,
    community_follower,
    community_moderator,
    community_person_ban,
    image_details,
    instance_block,
    local_user,
    local_user_language,
    person,
    person_block,
    person_post_aggregates,
    post,
    post_aggregates,
    post_hide,
    post_like,
    post_read,
    post_saved,
  },
  source::{
    community::{CommunityFollower, CommunityFollowerState},
    local_user::LocalUser,
    site::Site,
  },
  utils::{
    functions::coalesce,
    fuzzy_search,
    get_conn,
    limit_and_offset,
    now,
    Commented,
    DbConn,
    DbPool,
    ListFn,
    Queries,
    ReadFn,
    ReverseTimestampKey,
  },
  CommunityVisibility,
  ListingType,
  PostSortType,
};
use tracing::debug;
use PostSortType::*;

fn queries<'a>() -> Queries<
  impl ReadFn<'a, PostView, (PostId, Option<&'a LocalUser>, bool)>,
  impl ListFn<'a, PostView, (PostQuery<'a>, &'a Site)>,
> {
  let is_creator_banned_from_community = exists(
    community_person_ban::table.filter(
      post_aggregates::community_id
        .eq(community_person_ban::community_id)
        .and(community_person_ban::person_id.eq(post_aggregates::creator_id)),
    ),
  );

  let is_local_user_banned_from_community = |person_id| {
    exists(
      community_person_ban::table.filter(
        post_aggregates::community_id
          .eq(community_person_ban::community_id)
          .and(community_person_ban::person_id.eq(person_id)),
      ),
    )
  };

  let creator_is_moderator = exists(
    community_moderator::table.filter(
      post_aggregates::community_id
        .eq(community_moderator::community_id)
        .and(community_moderator::person_id.eq(post_aggregates::creator_id)),
    ),
  );

  let creator_is_admin = exists(
    local_user::table.filter(
      post_aggregates::creator_id
        .eq(local_user::person_id)
        .and(local_user::admin.eq(true)),
    ),
  );

  let is_read = |person_id| {
    exists(
      post_read::table.filter(
        post_aggregates::post_id
          .eq(post_read::post_id)
          .and(post_read::person_id.eq(person_id)),
      ),
    )
  };

  let is_hidden = |person_id| {
    exists(
      post_hide::table.filter(
        post_aggregates::post_id
          .eq(post_hide::post_id)
          .and(post_hide::person_id.eq(person_id)),
      ),
    )
  };

  let is_creator_blocked = |person_id| {
    exists(
      person_block::table.filter(
        post_aggregates::creator_id
          .eq(person_block::target_id)
          .and(person_block::person_id.eq(person_id)),
      ),
    )
  };

  let score = |person_id| {
    post_like::table
      .filter(
        post_aggregates::post_id
          .eq(post_like::post_id)
          .and(post_like::person_id.eq(person_id)),
      )
      .select(post_like::score.nullable())
      .single_value()
  };

  // TODO maybe this should go to localuser also
  let all_joins = move |query: post_aggregates::BoxedQuery<'a, Pg>,
                        my_person_id: Option<PersonId>| {
    let is_local_user_banned_from_community_selection: Box<
      dyn BoxableExpression<_, Pg, SqlType = sql_types::Bool>,
    > = if let Some(person_id) = my_person_id {
      Box::new(is_local_user_banned_from_community(person_id))
    } else {
      Box::new(false.into_sql::<sql_types::Bool>())
    };

    let is_read_selection: Box<dyn BoxableExpression<_, Pg, SqlType = sql_types::Bool>> =
      if let Some(person_id) = my_person_id {
        Box::new(is_read(person_id))
      } else {
        Box::new(false.into_sql::<sql_types::Bool>())
      };

    let is_hidden_selection: Box<dyn BoxableExpression<_, Pg, SqlType = sql_types::Bool>> =
      if let Some(person_id) = my_person_id {
        Box::new(is_hidden(person_id))
      } else {
        Box::new(false.into_sql::<sql_types::Bool>())
      };

    let is_creator_blocked_selection: Box<dyn BoxableExpression<_, Pg, SqlType = sql_types::Bool>> =
      if let Some(person_id) = my_person_id {
        Box::new(is_creator_blocked(person_id))
      } else {
        Box::new(false.into_sql::<sql_types::Bool>())
      };

    let subscribed_type_selection: Box<
      dyn BoxableExpression<
        _,
        Pg,
        SqlType = sql_types::Nullable<lemmy_db_schema::schema::sql_types::CommunityFollowerState>,
      >,
    > = if let Some(person_id) = my_person_id {
      Box::new(
        community_follower::table
          .filter(
            post_aggregates::community_id
              .eq(community_follower::community_id)
              .and(community_follower::person_id.eq(person_id)),
          )
          .select(CommunityFollower::select_subscribed_type())
          .single_value(),
      )
    } else {
      Box::new(None::<CommunityFollowerState>.into_sql::<sql_types::Nullable<lemmy_db_schema::schema::sql_types::CommunityFollowerState>>())
    };

    let score_selection: Box<
      dyn BoxableExpression<_, Pg, SqlType = sql_types::Nullable<sql_types::SmallInt>>,
    > = if let Some(person_id) = my_person_id {
      Box::new(score(person_id))
    } else {
      Box::new(None::<i16>.into_sql::<sql_types::Nullable<sql_types::SmallInt>>())
    };

    let read_comments: Box<
      dyn BoxableExpression<_, Pg, SqlType = sql_types::Nullable<sql_types::BigInt>>,
    > = if let Some(person_id) = my_person_id {
      Box::new(
        person_post_aggregates::table
          .filter(
            post_aggregates::post_id
              .eq(person_post_aggregates::post_id)
              .and(person_post_aggregates::person_id.eq(person_id)),
          )
          .select(person_post_aggregates::read_comments.nullable())
          .single_value(),
      )
    } else {
      Box::new(None::<i64>.into_sql::<sql_types::Nullable<sql_types::BigInt>>())
    };

    query
      .inner_join(person::table)
      .inner_join(community::table)
      .inner_join(post::table)
      .left_join(image_details::table.on(post::thumbnail_url.eq(image_details::link.nullable())))
      .left_join(
        post_saved::table.on(
          post_aggregates::post_id
            .eq(post_saved::post_id)
            .and(post_saved::person_id.eq(my_person_id.unwrap_or(PersonId(-1)))),
        ),
      )
      .select((
        post::all_columns,
        person::all_columns,
        community::all_columns,
        image_details::all_columns.nullable(),
        is_creator_banned_from_community,
        is_local_user_banned_from_community_selection,
        creator_is_moderator,
        creator_is_admin,
        post_aggregates::all_columns,
        subscribed_type_selection,
        post_saved::person_id.nullable().is_not_null(),
        is_read_selection,
        is_hidden_selection,
        is_creator_blocked_selection,
        score_selection,
        coalesce(
          post_aggregates::comments.nullable() - read_comments,
          post_aggregates::comments,
        ),
      ))
  };

  let read = move |mut conn: DbConn<'a>,
                   (post_id, my_local_user, is_mod_or_admin): (
    PostId,
    Option<&'a LocalUser>,
    bool,
  )| async move {
    // The left join below will return None in this case
    let my_person_id = my_local_user.person_id();
    let person_id_join = my_person_id.unwrap_or(PersonId(-1));

    let mut query = all_joins(
      post_aggregates::table
        .filter(post_aggregates::post_id.eq(post_id))
        .into_boxed(),
      my_person_id,
    );

    // Hide deleted and removed for non-admins or mods
    if !is_mod_or_admin {
      query = query
        .filter(
          community::removed
            .eq(false)
            .or(post::creator_id.eq(person_id_join)),
        )
        .filter(
          post::removed
            .eq(false)
            .or(post::creator_id.eq(person_id_join)),
        )
        // users can see their own deleted posts
        .filter(
          community::deleted
            .eq(false)
            .or(post::creator_id.eq(person_id_join)),
        )
        .filter(
          post::deleted
            .eq(false)
            .or(post::creator_id.eq(person_id_join)),
        )
        // private communities can only by browsed by accepted followers
        .filter(
          community::visibility
            .ne(CommunityVisibility::Private)
            .or(exists(
              community_follower::table.filter(
                post_aggregates::community_id
                  .eq(community_follower::community_id)
                  .and(
                    community_follower::person_id
                      .eq(my_local_user.map(|l| l.person_id).unwrap_or_default())
                      .and(community_follower::state.eq(CommunityFollowerState::Accepted)),
                  ),
              ),
            )),
        );
    }

    query = my_local_user.visible_communities_only(query);

    Commented::new(query)
      .text("PostView::read")
      .first(&mut conn)
      .await
  };

  let list = move |mut conn: DbConn<'a>, (options, site): (PostQuery<'a>, &'a Site)| async move {
    // The left join below will return None in this case
    let person_id_join = options.local_user.person_id().unwrap_or(PersonId(-1));
    let local_user_id_join = options
      .local_user
      .local_user_id()
      .unwrap_or(LocalUserId(-1));

    let mut query = all_joins(
      post_aggregates::table.into_boxed(),
      options.local_user.person_id(),
    );

    // hide posts from deleted communities
    query = query.filter(community::deleted.eq(false));

    // only creator can see deleted posts and unpublished scheduled posts
    if let Some(person_id) = options.local_user.person_id() {
      query = query.filter(post::deleted.eq(false).or(post::creator_id.eq(person_id)));
      query = query.filter(
        post::scheduled_publish_time
          .is_null()
          .or(post::creator_id.eq(person_id)),
      );
    } else {
      query = query
        .filter(post::deleted.eq(false))
        .filter(post::scheduled_publish_time.is_null());
    }

    // only show removed posts to admin when viewing user profile
    if !(options.creator_id.is_some() && options.local_user.is_admin()) {
      query = query
        .filter(community::removed.eq(false))
        .filter(post::removed.eq(false));
    }
    if let Some(community_id) = options.community_id {
      query = query.filter(post_aggregates::community_id.eq(community_id));
    }

    if let Some(creator_id) = options.creator_id {
      query = query.filter(post_aggregates::creator_id.eq(creator_id));
    }

    let is_subscribed = exists(
      community_follower::table.filter(
        post_aggregates::community_id
          .eq(community_follower::community_id)
          .and(community_follower::person_id.eq(person_id_join)),
      ),
    );
    match options.listing_type.unwrap_or_default() {
      ListingType::Subscribed => query = query.filter(is_subscribed),
      ListingType::Local => {
        query = query
          .filter(community::local.eq(true))
          .filter(community::hidden.eq(false).or(is_subscribed));
      }
      ListingType::All => query = query.filter(community::hidden.eq(false).or(is_subscribed)),
      ListingType::ModeratorView => {
        query = query.filter(exists(
          community_moderator::table.filter(
            post::community_id
              .eq(community_moderator::community_id)
              .and(community_moderator::person_id.eq(person_id_join)),
          ),
        ));
      }
    }

    if let Some(search_term) = &options.search_term {
      if options.url_only.unwrap_or_default() {
        query = query.filter(post::url.eq(search_term));
      } else {
        let searcher = fuzzy_search(search_term);
        let name_filter = post::name.ilike(searcher.clone());
        let body_filter = post::body.ilike(searcher.clone());
        query = if options.title_only.unwrap_or_default() {
          query.filter(name_filter)
        } else {
          query.filter(name_filter.or(body_filter))
        }
        .filter(not(post::removed.or(post::deleted)));
      }
    }

    if !options
      .show_nsfw
      .unwrap_or(options.local_user.show_nsfw(site))
    {
      query = query
        .filter(post::nsfw.eq(false))
        .filter(community::nsfw.eq(false));
    };

    if !options.local_user.show_bot_accounts() {
      query = query.filter(person::bot_account.eq(false));
    };

    // Filter to show only posts with no comments
    if options.no_comments_only.unwrap_or_default() {
      query = query.filter(post_aggregates::comments.eq(0));
    };

    // If its saved only, then filter, and order by the saved time, not the comment creation time.
    if options.saved_only.unwrap_or_default() {
      query = query
        .filter(post_saved::person_id.is_not_null())
        .then_order_by(post_saved::published.desc());
    }
    // Only hide the read posts, if the saved_only is false. Otherwise ppl with the hide_read
    // setting wont be able to see saved posts.
    else if !options
      .show_read
      .unwrap_or(options.local_user.show_read_posts())
    {
      // Do not hide read posts when it is a user profile view
      // Or, only hide read posts on non-profile views
      if let (None, Some(person_id)) = (options.creator_id, options.local_user.person_id()) {
        query = query.filter(not(is_read(person_id)));
      }
    }

    if !options.show_hidden.unwrap_or_default() {
      // If a creator id isn't given (IE its on home or community pages), hide the hidden posts
      if let (None, Some(person_id)) = (options.creator_id, options.local_user.person_id()) {
        query = query.filter(not(is_hidden(person_id)));
      }
    }

    if let Some(my_id) = options.local_user.person_id() {
      let not_creator_filter = post_aggregates::creator_id.ne(my_id);
      if options.liked_only.unwrap_or_default() {
        query = query.filter(not_creator_filter).filter(score(my_id).eq(1));
      } else if options.disliked_only.unwrap_or_default() {
        query = query.filter(not_creator_filter).filter(score(my_id).eq(-1));
      }
    };

    query = options.local_user.visible_communities_only(query);

    if !options.local_user.is_admin() {
      query = query.filter(
        community::visibility
          .ne(CommunityVisibility::Private)
          .or(exists(
            community_follower::table.filter(
              post_aggregates::community_id
                .eq(community_follower::community_id)
                .and(community_follower::person_id.eq(person_id_join))
                .and(community_follower::state.eq(CommunityFollowerState::Accepted)),
            ),
          )),
      );
    }

    // Dont filter blocks or missing languages for moderator view type
    if let (Some(person_id), false) = (
      options.local_user.person_id(),
      options.listing_type.unwrap_or_default() == ListingType::ModeratorView,
    ) {
      // Filter out the rows with missing languages
      query = query.filter(exists(
        local_user_language::table.filter(
          post::language_id
            .eq(local_user_language::language_id)
            .and(local_user_language::local_user_id.eq(local_user_id_join)),
        ),
      ));

      // Don't show blocked instances, communities or persons
      query = query.filter(not(exists(
        community_block::table.filter(
          post_aggregates::community_id
            .eq(community_block::community_id)
            .and(community_block::person_id.eq(person_id_join)),
        ),
      )));
      query = query.filter(not(exists(
        instance_block::table.filter(
          post_aggregates::instance_id
            .eq(instance_block::instance_id)
            .and(instance_block::person_id.eq(person_id_join)),
        ),
      )));
      query = query.filter(not(is_creator_blocked(person_id)));
    }

    let (limit, offset) = limit_and_offset(options.page, options.limit)?;
    query = query.limit(limit).offset(offset);

    let mut query = PaginatedQueryBuilder::new(query);

    let page_after = options.page_after.map(|c| c.0);
    let page_before_or_equal = options.page_before_or_equal.map(|c| c.0);

    if options.page_back.unwrap_or_default() {
      query = query
        .before(page_after)
        .after_or_equal(page_before_or_equal)
        .limit_and_offset_from_end();
    } else {
      query = query
        .after(page_after)
        .before_or_equal(page_before_or_equal);
    }

    // featured posts first
    query = if options.community_id.is_none() || options.community_id_just_for_prefetch {
      query.then_desc(key::featured_local)
    } else {
      query.then_desc(key::featured_community)
    };

    let time = |interval| post_aggregates::published.gt(now() - interval);

    // then use the main sort
    query = match options.sort.unwrap_or(Hot) {
      Active => query.then_desc(key::hot_rank_active),
      Hot => query.then_desc(key::hot_rank),
      Scaled => query.then_desc(key::scaled_rank),
      Controversial => query.then_desc(key::controversy_rank),
      New => query.then_desc(key::published),
      Old => query.then_desc(ReverseTimestampKey(key::published)),
      NewComments => query.then_desc(key::newest_comment_time),
      MostComments => query.then_desc(key::comments),
      TopAll => query.then_desc(key::score),
      TopYear => query.then_desc(key::score).filter(time(1.years())),
      TopMonth => query.then_desc(key::score).filter(time(1.months())),
      TopWeek => query.then_desc(key::score).filter(time(1.weeks())),
      TopDay => query.then_desc(key::score).filter(time(1.days())),
      TopHour => query.then_desc(key::score).filter(time(1.hours())),
      TopSixHour => query.then_desc(key::score).filter(time(6.hours())),
      TopTwelveHour => query.then_desc(key::score).filter(time(12.hours())),
      TopThreeMonths => query.then_desc(key::score).filter(time(3.months())),
      TopSixMonths => query.then_desc(key::score).filter(time(6.months())),
      TopNineMonths => query.then_desc(key::score).filter(time(9.months())),
    };

    // use publish as fallback. especially useful for hot rank which reaches zero after some days.
    // necessary because old posts can be fetched over federation and inserted with high post id
    query = match options.sort.unwrap_or(Hot) {
      // A second time-based sort would not be very useful
      New | Old | NewComments => query,
      _ => query.then_desc(key::published),
    };

    // finally use unique post id as tie breaker
    query = query.then_desc(key::post_id);

    // Not done by debug_query
    let query = query.as_query();

    debug!("Post View Query: {:?}", debug_query::<Pg, _>(&query));

    Commented::new(query)
      .text("PostQuery::list")
      .text_if(
        "getting upper bound for next query",
        options.community_id_just_for_prefetch,
      )
      .load::<PostView>(&mut conn)
      .await
  };

  Queries::new(read, list)
}

impl PostView {
  pub async fn read<'a>(
    pool: &mut DbPool<'_>,
    post_id: PostId,
    my_local_user: Option<&'a LocalUser>,
    is_mod_or_admin: bool,
  ) -> Result<Self, Error> {
    queries()
      .read(pool, (post_id, my_local_user, is_mod_or_admin))
      .await
  }
}

impl PaginationCursor {
  // get cursor for page that starts immediately after the given post
  pub fn after_post(view: &PostView) -> PaginationCursor {
    // hex encoding to prevent ossification
    PaginationCursor(format!("P{:x}", view.counts.post_id.0))
  }
  pub async fn read(&self, pool: &mut DbPool<'_>) -> Result<PaginationCursorData, Error> {
    let err_msg = || Error::QueryBuilderError("Could not parse pagination token".into());
    let token = PostAggregates::read(
      pool,
      PostId(
        self
          .0
          .get(1..)
          .and_then(|e| i32::from_str_radix(e, 16).ok())
          .ok_or_else(err_msg)?,
      ),
    )
    .await?;

    Ok(PaginationCursorData(token))
  }
}

// currently we use a postaggregates struct as the pagination token.
// we only use some of the properties of the post aggregates, depending on which sort type we page
// by
#[derive(Clone)]
pub struct PaginationCursorData(PostAggregates);

#[derive(Clone, Default)]
pub struct PostQuery<'a> {
  pub listing_type: Option<ListingType>,
  pub sort: Option<PostSortType>,
  pub creator_id: Option<PersonId>,
  pub community_id: Option<CommunityId>,
  // if true, the query should be handled as if community_id was not given except adding the
  // literal filter
  pub community_id_just_for_prefetch: bool,
  pub local_user: Option<&'a LocalUser>,
  pub search_term: Option<String>,
  pub url_only: Option<bool>,
  pub saved_only: Option<bool>,
  pub liked_only: Option<bool>,
  pub disliked_only: Option<bool>,
  pub title_only: Option<bool>,
  pub page: Option<i64>,
  pub limit: Option<i64>,
  pub page_after: Option<PaginationCursorData>,
  pub page_before_or_equal: Option<PaginationCursorData>,
  pub page_back: Option<bool>,
  pub show_hidden: Option<bool>,
  pub show_read: Option<bool>,
  pub show_nsfw: Option<bool>,
  pub no_comments_only: Option<bool>,
}

impl<'a> PostQuery<'a> {
  async fn prefetch_upper_bound_for_page_before(
    &self,
    site: &Site,
    pool: &mut DbPool<'_>,
  ) -> Result<Option<PostQuery<'a>>, Error> {
    // first get one page for the most popular community to get an upper bound for the page end for
    // the real query. the reason this is needed is that when fetching posts for a single
    // community PostgreSQL can optimize the query to use an index on e.g. (=, >=, >=, >=) and
    // fetch only LIMIT rows but for the followed-communities query it has to query the index on
    // (IN, >=, >=, >=) which it currently can't do at all (as of PG 16). see the discussion
    // here: https://github.com/LemmyNet/lemmy/issues/2877#issuecomment-1673597190
    //
    // the results are correct no matter which community we fetch these for, since it basically
    // covers the "worst case" of the whole page consisting of posts from one community
    // but using the largest community decreases the pagination-frame so make the real query more
    // efficient.
    use lemmy_db_schema::schema::{
      community_aggregates::dsl::{community_aggregates, community_id, users_active_month},
      community_follower::dsl::{
        community_follower,
        community_id as follower_community_id,
        person_id,
      },
    };
    let (limit, offset) = limit_and_offset(self.page, self.limit)?;
    if offset != 0 && self.page_after.is_some() {
      return Err(Error::QueryBuilderError(
        "legacy pagination cannot be combined with v2 pagination".into(),
      ));
    }
    let self_person_id = self.local_user.expect("part of the above if").person_id;
    let largest_subscribed = {
      let conn = &mut get_conn(pool).await?;
      community_follower
        .filter(person_id.eq(self_person_id))
        .inner_join(community_aggregates.on(community_id.eq(follower_community_id)))
        .order_by(users_active_month.desc())
        .select(community_id)
        .limit(1)
        .get_result::<CommunityId>(conn)
        .await
        .optional()?
    };
    let Some(largest_subscribed) = largest_subscribed else {
      // nothing subscribed to? no posts
      return Ok(None);
    };

    let mut v = queries()
      .list(
        pool,
        (
          PostQuery {
            community_id: Some(largest_subscribed),
            community_id_just_for_prefetch: true,
            ..self.clone()
          },
          site,
        ),
      )
      .await?;
    // take last element of array. if this query returned less than LIMIT elements,
    // the heuristic is invalid since we can't guarantee the full query will return >= LIMIT results
    // (return original query)
    if (v.len() as i64) < limit {
      Ok(Some(self.clone()))
    } else {
      let item = if self.page_back.unwrap_or_default() {
        // for backward pagination, get first element instead
        v.into_iter().next()
      } else {
        v.pop()
      };
      let limit_cursor = Some(PaginationCursorData(item.expect("else case").counts));
      Ok(Some(PostQuery {
        page_before_or_equal: limit_cursor,
        ..self.clone()
      }))
    }
  }

  pub async fn list(self, site: &Site, pool: &mut DbPool<'_>) -> Result<Vec<PostView>, Error> {
    if self.listing_type == Some(ListingType::Subscribed)
      && self.community_id.is_none()
      && self.local_user.is_some()
      && self.page_before_or_equal.is_none()
    {
      if let Some(query) = self
        .prefetch_upper_bound_for_page_before(site, pool)
        .await?
      {
        queries().list(pool, (query, site)).await
      } else {
        Ok(vec![])
      }
    } else {
      queries().list(pool, (self, site)).await
    }
  }
}

#[cfg(test)]
mod tests {
  use crate::{
    post_view::{PaginationCursorData, PostQuery, PostView},
    structs::LocalUserView,
  };
  use chrono::Utc;
  use diesel_async::SimpleAsyncConnection;
  use lemmy_db_schema::{
    aggregates::structs::PostAggregates,
    impls::actor_language::UNDETERMINED_ID,
    newtypes::LanguageId,
    source::{
      actor_language::LocalUserLanguage,
      comment::{Comment, CommentInsertForm},
      community::{
        Community,
        CommunityFollower,
        CommunityFollowerForm,
        CommunityFollowerState,
        CommunityInsertForm,
        CommunityModerator,
        CommunityModeratorForm,
        CommunityPersonBan,
        CommunityPersonBanForm,
        CommunityUpdateForm,
      },
      community_block::{CommunityBlock, CommunityBlockForm},
      instance::Instance,
      instance_block::{InstanceBlock, InstanceBlockForm},
      language::Language,
      local_user::{LocalUser, LocalUserInsertForm, LocalUserUpdateForm},
      local_user_vote_display_mode::LocalUserVoteDisplayMode,
      person::{Person, PersonInsertForm},
      person_block::{PersonBlock, PersonBlockForm},
      post::{
        Post,
        PostHide,
        PostInsertForm,
        PostLike,
        PostLikeForm,
        PostRead,
        PostSaved,
        PostSavedForm,
        PostUpdateForm,
      },
      site::Site,
    },
    traits::{Bannable, Blockable, Crud, Followable, Joinable, Likeable, Saveable},
    utils::{build_db_pool, build_db_pool_for_tests, get_conn, DbPool, RANK_DEFAULT},
    CommunityVisibility,
    PostSortType,
    SubscribedType,
  };
  use lemmy_utils::error::LemmyResult;
  use pretty_assertions::assert_eq;
  use serial_test::serial;
  use std::{
    collections::HashSet,
    time::{Duration, Instant},
  };
  use url::Url;

  const POST_WITH_ANOTHER_TITLE: &str = "Another title";
  const POST_BY_BLOCKED_PERSON: &str = "post by blocked person";
  const POST_BY_BOT: &str = "post by bot";
  const POST: &str = "post";

  fn names(post_views: &[PostView]) -> Vec<&str> {
    post_views.iter().map(|i| i.post.name.as_str()).collect()
  }

  struct Data {
    inserted_instance: Instance,
    local_user_view: LocalUserView,
    blocked_local_user_view: LocalUserView,
    inserted_bot: Person,
    inserted_community: Community,
    inserted_post: Post,
    inserted_bot_post: Post,
    site: Site,
  }

  impl Data {
    fn default_post_query(&self) -> PostQuery<'_> {
      PostQuery {
        sort: Some(PostSortType::New),
        local_user: Some(&self.local_user_view.local_user),
        ..Default::default()
      }
    }
  }

  async fn init_data(pool: &mut DbPool<'_>) -> LemmyResult<Data> {
    let inserted_instance = Instance::read_or_create(pool, "my_domain.tld".to_string()).await?;

    let new_person = PersonInsertForm::test_form(inserted_instance.id, "tegan");

    let inserted_person = Person::create(pool, &new_person).await?;

    let local_user_form = LocalUserInsertForm {
      admin: Some(true),
      ..LocalUserInsertForm::test_form(inserted_person.id)
    };
    let inserted_local_user = LocalUser::create(pool, &local_user_form, vec![]).await?;

    let new_bot = PersonInsertForm {
      bot_account: Some(true),
      ..PersonInsertForm::test_form(inserted_instance.id, "mybot")
    };

    let inserted_bot = Person::create(pool, &new_bot).await?;

    let new_community = CommunityInsertForm::new(
      inserted_instance.id,
      "test_community_3".to_string(),
      "nada".to_owned(),
      "pubkey".to_string(),
    );
    let inserted_community = Community::create(pool, &new_community).await?;

    // Test a person block, make sure the post query doesn't include their post
    let blocked_person = PersonInsertForm::test_form(inserted_instance.id, "john");

    let inserted_blocked_person = Person::create(pool, &blocked_person).await?;

    let inserted_blocked_local_user = LocalUser::create(
      pool,
      &LocalUserInsertForm::test_form(inserted_blocked_person.id),
      vec![],
    )
    .await?;

    let post_from_blocked_person = PostInsertForm {
      language_id: Some(LanguageId(1)),
      ..PostInsertForm::new(
        POST_BY_BLOCKED_PERSON.to_string(),
        inserted_blocked_person.id,
        inserted_community.id,
      )
    };
    Post::create(pool, &post_from_blocked_person).await?;

    // block that person
    let person_block = PersonBlockForm {
      person_id: inserted_person.id,
      target_id: inserted_blocked_person.id,
    };

    PersonBlock::block(pool, &person_block).await?;

    // A sample post
    let new_post = PostInsertForm {
      language_id: Some(LanguageId(47)),
      ..PostInsertForm::new(POST.to_string(), inserted_person.id, inserted_community.id)
    };
    let inserted_post = Post::create(pool, &new_post).await?;

    let new_bot_post = PostInsertForm::new(
      POST_BY_BOT.to_string(),
      inserted_bot.id,
      inserted_community.id,
    );
    let inserted_bot_post = Post::create(pool, &new_bot_post).await?;

    let local_user_view = LocalUserView {
      local_user: inserted_local_user,
      local_user_vote_display_mode: LocalUserVoteDisplayMode::default(),
      person: inserted_person,
      counts: Default::default(),
    };
    let blocked_local_user_view = LocalUserView {
      local_user: inserted_blocked_local_user,
      local_user_vote_display_mode: LocalUserVoteDisplayMode::default(),
      person: inserted_blocked_person,
      counts: Default::default(),
    };

    let site = Site {
      id: Default::default(),
      name: String::new(),
      sidebar: None,
      published: Default::default(),
      updated: None,
      icon: None,
      banner: None,
      description: None,
      actor_id: Url::parse("http://example.com")?.into(),
      last_refreshed_at: Default::default(),
      inbox_url: Url::parse("http://example.com")?.into(),
      private_key: None,
      public_key: String::new(),
      instance_id: Default::default(),
      content_warning: None,
    };

    Ok(Data {
      inserted_instance,
      local_user_view,
      blocked_local_user_view,
      inserted_bot,
      inserted_community,
      inserted_post,
      inserted_bot_post,
      site,
    })
  }

  #[tokio::test]
  #[serial]
  async fn post_listing_with_person() -> LemmyResult<()> {
    let pool = &build_db_pool()?;
    let pool = &mut pool.into();
    let mut data = init_data(pool).await?;

    let local_user_form = LocalUserUpdateForm {
      show_bot_accounts: Some(false),
      ..Default::default()
    };
    LocalUser::update(pool, data.local_user_view.local_user.id, &local_user_form).await?;
    data.local_user_view.local_user.show_bot_accounts = false;

    let read_post_listing = PostQuery {
      community_id: Some(data.inserted_community.id),
      ..data.default_post_query()
    }
    .list(&data.site, pool)
    .await?;

    let post_listing_single_with_person = PostView::read(
      pool,
      data.inserted_post.id,
      Some(&data.local_user_view.local_user),
      false,
    )
    .await?;

    let expected_post_listing_with_user = expected_post_view(&data, pool).await?;

    // Should be only one person, IE the bot post, and blocked should be missing
    assert_eq!(
      vec![post_listing_single_with_person.clone()],
      read_post_listing
    );
    assert_eq!(
      expected_post_listing_with_user,
      post_listing_single_with_person
    );

    let local_user_form = LocalUserUpdateForm {
      show_bot_accounts: Some(true),
      ..Default::default()
    };
    LocalUser::update(pool, data.local_user_view.local_user.id, &local_user_form).await?;
    data.local_user_view.local_user.show_bot_accounts = true;

    let post_listings_with_bots = PostQuery {
      community_id: Some(data.inserted_community.id),
      ..data.default_post_query()
    }
    .list(&data.site, pool)
    .await?;
    // should include bot post which has "undetermined" language
    assert_eq!(vec![POST_BY_BOT, POST], names(&post_listings_with_bots));

    cleanup(data, pool).await
  }

  #[tokio::test]
  #[serial]
  async fn post_listing_no_person() -> LemmyResult<()> {
    let pool = &build_db_pool()?;
    let pool = &mut pool.into();
    let data = init_data(pool).await?;

    let read_post_listing_multiple_no_person = PostQuery {
      community_id: Some(data.inserted_community.id),
      local_user: None,
      ..data.default_post_query()
    }
    .list(&data.site, pool)
    .await?;

    let read_post_listing_single_no_person =
      PostView::read(pool, data.inserted_post.id, None, false).await?;

    let expected_post_listing_no_person = expected_post_view(&data, pool).await?;

    // Should be 2 posts, with the bot post, and the blocked
    assert_eq!(
      vec![POST_BY_BOT, POST, POST_BY_BLOCKED_PERSON],
      names(&read_post_listing_multiple_no_person)
    );

    assert_eq!(
      Some(&expected_post_listing_no_person),
      read_post_listing_multiple_no_person.get(1)
    );
    assert_eq!(
      expected_post_listing_no_person,
      read_post_listing_single_no_person
    );

    cleanup(data, pool).await
  }

  #[tokio::test]
  #[serial]
  async fn post_listing_title_only() -> LemmyResult<()> {
    let pool = &build_db_pool()?;
    let pool = &mut pool.into();
    let data = init_data(pool).await?;

    // A post which contains the search them 'Post' not in the title (but in the body)
    let new_post = PostInsertForm {
      language_id: Some(LanguageId(47)),
      body: Some("Post".to_string()),
      ..PostInsertForm::new(
        POST_WITH_ANOTHER_TITLE.to_string(),
        data.local_user_view.person.id,
        data.inserted_community.id,
      )
    };

    let inserted_post = Post::create(pool, &new_post).await?;

    let read_post_listing_by_title_only = PostQuery {
      community_id: Some(data.inserted_community.id),
      local_user: None,
      search_term: Some("Post".to_string()),
      title_only: Some(true),
      ..data.default_post_query()
    }
    .list(&data.site, pool)
    .await?;

    let read_post_listing = PostQuery {
      community_id: Some(data.inserted_community.id),
      local_user: None,
      search_term: Some("Post".to_string()),
      ..data.default_post_query()
    }
    .list(&data.site, pool)
    .await?;

    // Should be 4 posts when we do not search for title only
    assert_eq!(
      vec![
        POST_WITH_ANOTHER_TITLE,
        POST_BY_BOT,
        POST,
        POST_BY_BLOCKED_PERSON
      ],
      names(&read_post_listing)
    );

    // Should be 3 posts when we search for title only
    assert_eq!(
      vec![POST_BY_BOT, POST, POST_BY_BLOCKED_PERSON],
      names(&read_post_listing_by_title_only)
    );
    Post::delete(pool, inserted_post.id).await?;
    cleanup(data, pool).await
  }

  #[tokio::test]
  #[serial]
  async fn post_listing_block_community() -> LemmyResult<()> {
    let pool = &build_db_pool()?;
    let pool = &mut pool.into();
    let data = init_data(pool).await?;

    let community_block = CommunityBlockForm {
      person_id: data.local_user_view.person.id,
      community_id: data.inserted_community.id,
    };
    CommunityBlock::block(pool, &community_block).await?;

    let read_post_listings_with_person_after_block = PostQuery {
      community_id: Some(data.inserted_community.id),
      ..data.default_post_query()
    }
    .list(&data.site, pool)
    .await?;
    // Should be 0 posts after the community block
    assert_eq!(read_post_listings_with_person_after_block, vec![]);

    CommunityBlock::unblock(pool, &community_block).await?;
    cleanup(data, pool).await
  }

  #[tokio::test]
  #[serial]
  async fn post_listing_like() -> LemmyResult<()> {
    let pool = &build_db_pool()?;
    let pool = &mut pool.into();
    let mut data = init_data(pool).await?;

    let post_like_form = PostLikeForm {
      post_id: data.inserted_post.id,
      person_id: data.local_user_view.person.id,
      score: 1,
    };

    let inserted_post_like = PostLike::like(pool, &post_like_form).await?;

    let expected_post_like = PostLike {
      post_id: data.inserted_post.id,
      person_id: data.local_user_view.person.id,
      published: inserted_post_like.published,
      score: 1,
    };
    assert_eq!(expected_post_like, inserted_post_like);

    let post_listing_single_with_person = PostView::read(
      pool,
      data.inserted_post.id,
      Some(&data.local_user_view.local_user),
      false,
    )
    .await?;

    let mut expected_post_with_upvote = expected_post_view(&data, pool).await?;
    expected_post_with_upvote.my_vote = Some(1);
    expected_post_with_upvote.counts.score = 1;
    expected_post_with_upvote.counts.upvotes = 1;
    assert_eq!(expected_post_with_upvote, post_listing_single_with_person);

    let local_user_form = LocalUserUpdateForm {
      show_bot_accounts: Some(false),
      ..Default::default()
    };
    LocalUser::update(pool, data.local_user_view.local_user.id, &local_user_form).await?;
    data.local_user_view.local_user.show_bot_accounts = false;

    let read_post_listing = PostQuery {
      community_id: Some(data.inserted_community.id),
      ..data.default_post_query()
    }
    .list(&data.site, pool)
    .await?;
    assert_eq!(vec![expected_post_with_upvote], read_post_listing);

    let like_removed =
      PostLike::remove(pool, data.local_user_view.person.id, data.inserted_post.id).await?;
    assert_eq!(1, like_removed);
    cleanup(data, pool).await
  }

  #[tokio::test]
  #[serial]
  async fn post_listing_liked_only() -> LemmyResult<()> {
    let pool = &build_db_pool()?;
    let pool = &mut pool.into();
    let data = init_data(pool).await?;

    // Like both the bot post, and your own
    // The liked_only should not show your own post
    let post_like_form = PostLikeForm {
      post_id: data.inserted_post.id,
      person_id: data.local_user_view.person.id,
      score: 1,
    };
    PostLike::like(pool, &post_like_form).await?;

    let bot_post_like_form = PostLikeForm {
      post_id: data.inserted_bot_post.id,
      person_id: data.local_user_view.person.id,
      score: 1,
    };
    PostLike::like(pool, &bot_post_like_form).await?;

    // Read the liked only
    let read_liked_post_listing = PostQuery {
      community_id: Some(data.inserted_community.id),
      liked_only: Some(true),
      ..data.default_post_query()
    }
    .list(&data.site, pool)
    .await?;

    // This should only include the bot post, not the one you created
    assert_eq!(vec![POST_BY_BOT], names(&read_liked_post_listing));

    let read_disliked_post_listing = PostQuery {
      community_id: Some(data.inserted_community.id),
      disliked_only: Some(true),
      ..data.default_post_query()
    }
    .list(&data.site, pool)
    .await?;

    // Should be no posts
    assert_eq!(read_disliked_post_listing, vec![]);

    cleanup(data, pool).await
  }

  #[tokio::test]
  #[serial]
  async fn post_listing_saved_only() -> LemmyResult<()> {
    let pool = &build_db_pool()?;
    let pool = &mut pool.into();
    let data = init_data(pool).await?;

    // Save only the bot post
    // The saved_only should only show the bot post
    let post_save_form = PostSavedForm {
      post_id: data.inserted_bot_post.id,
      person_id: data.local_user_view.person.id,
    };
    PostSaved::save(pool, &post_save_form).await?;

    // Read the saved only
    let read_saved_post_listing = PostQuery {
      community_id: Some(data.inserted_community.id),
      saved_only: Some(true),
      ..data.default_post_query()
    }
    .list(&data.site, pool)
    .await?;

    // This should only include the bot post, not the one you created
    assert_eq!(vec![POST_BY_BOT], names(&read_saved_post_listing));

    cleanup(data, pool).await
  }

  #[tokio::test]
  #[serial]
  async fn creator_info() -> LemmyResult<()> {
    let pool = &build_db_pool()?;
    let pool = &mut pool.into();
    let data = init_data(pool).await?;

    // Make one of the inserted persons a moderator
    let person_id = data.local_user_view.person.id;
    let community_id = data.inserted_community.id;
    let form = CommunityModeratorForm {
      community_id,
      person_id,
    };
    CommunityModerator::join(pool, &form).await?;

    let post_listing = PostQuery {
      community_id: Some(data.inserted_community.id),
      ..data.default_post_query()
    }
    .list(&data.site, pool)
    .await?
    .into_iter()
    .map(|p| (p.creator.name, p.creator_is_moderator, p.creator_is_admin))
    .collect::<Vec<_>>();

    let expected_post_listing = vec![
      ("mybot".to_owned(), false, false),
      ("tegan".to_owned(), true, true),
    ];

    assert_eq!(expected_post_listing, post_listing);

    cleanup(data, pool).await
  }

  #[tokio::test]
  #[serial]
  async fn post_listing_person_language() -> LemmyResult<()> {
    const EL_POSTO: &str = "el posto";

    let pool = &build_db_pool()?;
    let pool = &mut pool.into();
    let data = init_data(pool).await?;

    let spanish_id = Language::read_id_from_code(pool, "es").await?;

    let french_id = Language::read_id_from_code(pool, "fr").await?;

    let post_spanish = PostInsertForm {
      language_id: Some(spanish_id),
      ..PostInsertForm::new(
        EL_POSTO.to_string(),
        data.local_user_view.person.id,
        data.inserted_community.id,
      )
    };
    Post::create(pool, &post_spanish).await?;

    let post_listings_all = data.default_post_query().list(&data.site, pool).await?;

    // no language filters specified, all posts should be returned
    assert_eq!(vec![EL_POSTO, POST_BY_BOT, POST], names(&post_listings_all));

    LocalUserLanguage::update(pool, vec![french_id], data.local_user_view.local_user.id).await?;

    let post_listing_french = data.default_post_query().list(&data.site, pool).await?;

    // only one post in french and one undetermined should be returned
    assert_eq!(vec![POST_BY_BOT, POST], names(&post_listing_french));
    assert_eq!(
      Some(french_id),
      post_listing_french.get(1).map(|p| p.post.language_id)
    );

    LocalUserLanguage::update(
      pool,
      vec![french_id, UNDETERMINED_ID],
      data.local_user_view.local_user.id,
    )
    .await?;
    let post_listings_french_und = data
      .default_post_query()
      .list(&data.site, pool)
      .await?
      .into_iter()
      .map(|p| (p.post.name, p.post.language_id))
      .collect::<Vec<_>>();
    let expected_post_listings_french_und = vec![
      (POST_BY_BOT.to_owned(), UNDETERMINED_ID),
      (POST.to_owned(), french_id),
    ];

    // french post and undetermined language post should be returned
    assert_eq!(expected_post_listings_french_und, post_listings_french_und);

    cleanup(data, pool).await
  }

  #[tokio::test]
  #[serial]
  async fn post_listings_removed() -> LemmyResult<()> {
    let pool = &build_db_pool()?;
    let pool = &mut pool.into();
    let mut data = init_data(pool).await?;

    // Remove the post
    Post::update(
      pool,
      data.inserted_bot_post.id,
      &PostUpdateForm {
        removed: Some(true),
        ..Default::default()
      },
    )
    .await?;

    // Make sure you don't see the removed post in the results
    let post_listings_no_admin = data.default_post_query().list(&data.site, pool).await?;
    assert_eq!(vec![POST], names(&post_listings_no_admin));

    // Removed bot post is shown to admins on its profile page
    data.local_user_view.local_user.admin = true;
    let post_listings_is_admin = PostQuery {
      creator_id: Some(data.inserted_bot.id),
      ..data.default_post_query()
    }
    .list(&data.site, pool)
    .await?;
    assert_eq!(vec![POST_BY_BOT], names(&post_listings_is_admin));

    cleanup(data, pool).await
  }

  #[tokio::test]
  #[serial]
  async fn post_listings_deleted() -> LemmyResult<()> {
    let pool = &build_db_pool()?;
    let pool = &mut pool.into();
    let data = init_data(pool).await?;

    // Delete the post
    Post::update(
      pool,
      data.inserted_post.id,
      &PostUpdateForm {
        deleted: Some(true),
        ..Default::default()
      },
    )
    .await?;

    // Deleted post is only shown to creator
    for (local_user, expect_contains_deleted) in [
      (None, false),
      (Some(&data.blocked_local_user_view.local_user), false),
      (Some(&data.local_user_view.local_user), true),
    ] {
      let contains_deleted = PostQuery {
        local_user,
        ..data.default_post_query()
      }
      .list(&data.site, pool)
      .await?
      .iter()
      .any(|p| p.post.id == data.inserted_post.id);

      assert_eq!(expect_contains_deleted, contains_deleted);
    }

    cleanup(data, pool).await
  }

  #[tokio::test]
  #[serial]
  async fn post_listings_hidden_community() -> LemmyResult<()> {
    let pool = &build_db_pool()?;
    let pool = &mut pool.into();
    let data = init_data(pool).await?;

    Community::update(
      pool,
      data.inserted_community.id,
      &CommunityUpdateForm {
        hidden: Some(true),
        ..Default::default()
      },
    )
    .await?;

    let posts = PostQuery::default().list(&data.site, pool).await?;
    assert!(posts.is_empty());

    let posts = data.default_post_query().list(&data.site, pool).await?;
    assert!(posts.is_empty());

    // Follow the community
    let form = CommunityFollowerForm {
      state: Some(CommunityFollowerState::Accepted),
      ..CommunityFollowerForm::new(data.inserted_community.id, data.local_user_view.person.id)
    };
    CommunityFollower::follow(pool, &form).await?;

    let posts = data.default_post_query().list(&data.site, pool).await?;
    assert!(!posts.is_empty());

    cleanup(data, pool).await
  }

  #[tokio::test]
  #[serial]
  async fn post_listing_instance_block() -> LemmyResult<()> {
    const POST_FROM_BLOCKED_INSTANCE: &str = "post on blocked instance";

    let pool = &build_db_pool()?;
    let pool = &mut pool.into();
    let data = init_data(pool).await?;

    let blocked_instance = Instance::read_or_create(pool, "another_domain.tld".to_string()).await?;

    let community_form = CommunityInsertForm::new(
      blocked_instance.id,
      "test_community_4".to_string(),
      "none".to_owned(),
      "pubkey".to_string(),
    );
    let inserted_community = Community::create(pool, &community_form).await?;

    let post_form = PostInsertForm {
      language_id: Some(LanguageId(1)),
      ..PostInsertForm::new(
        POST_FROM_BLOCKED_INSTANCE.to_string(),
        data.inserted_bot.id,
        inserted_community.id,
      )
    };
    let post_from_blocked_instance = Post::create(pool, &post_form).await?;

    // no instance block, should return all posts
    let post_listings_all = data.default_post_query().list(&data.site, pool).await?;
    assert_eq!(
      vec![POST_FROM_BLOCKED_INSTANCE, POST_BY_BOT, POST],
      names(&post_listings_all)
    );

    // block the instance
    let block_form = InstanceBlockForm {
      person_id: data.local_user_view.person.id,
      instance_id: blocked_instance.id,
    };
    InstanceBlock::block(pool, &block_form).await?;

    // now posts from communities on that instance should be hidden
    let post_listings_blocked = data.default_post_query().list(&data.site, pool).await?;
    assert_eq!(vec![POST_BY_BOT, POST], names(&post_listings_blocked));
    assert!(post_listings_blocked
      .iter()
      .all(|p| p.post.id != post_from_blocked_instance.id));

    // after unblocking it should return all posts again
    InstanceBlock::unblock(pool, &block_form).await?;
    let post_listings_blocked = data.default_post_query().list(&data.site, pool).await?;
    assert_eq!(
      vec![POST_FROM_BLOCKED_INSTANCE, POST_BY_BOT, POST],
      names(&post_listings_blocked)
    );

    Instance::delete(pool, blocked_instance.id).await?;
    cleanup(data, pool).await
  }

  #[tokio::test]
  #[serial]
  async fn pagination_includes_each_post_once() -> LemmyResult<()> {
    let pool = &build_db_pool()?;
    let pool = &mut pool.into();
    let data = init_data(pool).await?;

    let community_form = CommunityInsertForm::new(
      data.inserted_instance.id,
      "yes".to_string(),
      "yes".to_owned(),
      "pubkey".to_string(),
    );
    let inserted_community = Community::create(pool, &community_form).await?;

    let mut inserted_post_ids = vec![];
    let mut inserted_comment_ids = vec![];

    // Create 150 posts with varying non-correlating values for publish date, number of comments,
    // and featured
    for comments in 0..10 {
      for _ in 0..15 {
        let post_form = PostInsertForm {
          featured_local: Some((comments % 2) == 0),
          featured_community: Some((comments % 2) == 0),
          published: Some(Utc::now() - Duration::from_secs(comments % 3)),
          ..PostInsertForm::new(
            "keep Christ in Christmas".to_owned(),
            data.local_user_view.person.id,
            inserted_community.id,
          )
        };
        let inserted_post = Post::create(pool, &post_form).await?;
        inserted_post_ids.push(inserted_post.id);

        for _ in 0..comments {
          let comment_form = CommentInsertForm::new(
            data.local_user_view.person.id,
            inserted_post.id,
            "yes".to_owned(),
          );
          let inserted_comment = Comment::create(pool, &comment_form, None).await?;
          inserted_comment_ids.push(inserted_comment.id);
        }
      }
    }

    let options = PostQuery {
      community_id: Some(inserted_community.id),
      sort: Some(PostSortType::MostComments),
      limit: Some(10),
      ..Default::default()
    };

    let mut listed_post_ids = vec![];
    let mut page_after = None;
    loop {
      let post_listings = PostQuery {
        page_after,
        ..options.clone()
      }
      .list(&data.site, pool)
      .await?;

      listed_post_ids.extend(post_listings.iter().map(|p| p.post.id));

      if let Some(p) = post_listings.into_iter().last() {
        page_after = Some(PaginationCursorData(p.counts));
      } else {
        break;
      }
    }

    // Check that backward pagination matches forward pagination
    let mut listed_post_ids_forward = listed_post_ids.clone();
    let mut page_before = None;
    loop {
      let post_listings = PostQuery {
        page_after: page_before,
        page_back: Some(true),
        ..options.clone()
      }
      .list(&data.site, pool)
      .await?;

      let listed_post_ids = post_listings.iter().map(|p| p.post.id).collect::<Vec<_>>();

      let index = listed_post_ids_forward.len() - listed_post_ids.len();
      assert_eq!(
        listed_post_ids_forward.get(index..),
        listed_post_ids.get(..)
      );
      listed_post_ids_forward.truncate(index);

      if let Some(p) = post_listings.into_iter().next() {
        page_before = Some(PaginationCursorData(p.counts));
      } else {
        break;
      }
    }

    inserted_post_ids.sort_unstable_by_key(|id| id.0);
    listed_post_ids.sort_unstable_by_key(|id| id.0);

    assert_eq!(inserted_post_ids, listed_post_ids);

    Community::delete(pool, inserted_community.id).await?;
    cleanup(data, pool).await
  }

  #[tokio::test]
  #[serial]
  async fn post_listings_hide_read() -> LemmyResult<()> {
    let pool = &build_db_pool()?;
    let pool = &mut pool.into();
    let mut data = init_data(pool).await?;

    // Make sure local user hides read posts
    let local_user_form = LocalUserUpdateForm {
      show_read_posts: Some(false),
      ..Default::default()
    };
    LocalUser::update(pool, data.local_user_view.local_user.id, &local_user_form).await?;
    data.local_user_view.local_user.show_read_posts = false;

    // Mark a post as read
    PostRead::mark_as_read(
      pool,
      HashSet::from([data.inserted_bot_post.id]),
      data.local_user_view.person.id,
    )
    .await?;

    // Make sure you don't see the read post in the results
    let post_listings_hide_read = data.default_post_query().list(&data.site, pool).await?;
    assert_eq!(vec![POST], names(&post_listings_hide_read));

    // Test with the show_read override as true
    let post_listings_show_read_true = PostQuery {
      show_read: Some(true),
      ..data.default_post_query()
    }
    .list(&data.site, pool)
    .await?;
    assert_eq!(
      vec![POST_BY_BOT, POST],
      names(&post_listings_show_read_true)
    );

    // Test with the show_read override as false
    let post_listings_show_read_false = PostQuery {
      show_read: Some(false),
      ..data.default_post_query()
    }
    .list(&data.site, pool)
    .await?;
    assert_eq!(vec![POST], names(&post_listings_show_read_false));
    cleanup(data, pool).await
  }

  #[tokio::test]
  #[serial]
  async fn post_listings_hide_hidden() -> LemmyResult<()> {
    let pool = &build_db_pool()?;
    let pool = &mut pool.into();
    let data = init_data(pool).await?;

    // Mark a post as hidden
    PostHide::hide(
      pool,
      HashSet::from([data.inserted_bot_post.id]),
      data.local_user_view.person.id,
    )
    .await?;

    // Make sure you don't see the hidden post in the results
    let post_listings_hide_hidden = data.default_post_query().list(&data.site, pool).await?;
    assert_eq!(vec![POST], names(&post_listings_hide_hidden));

    // Make sure it does come back with the show_hidden option
    let post_listings_show_hidden = PostQuery {
      sort: Some(PostSortType::New),
      local_user: Some(&data.local_user_view.local_user),
      show_hidden: Some(true),
      ..Default::default()
    }
    .list(&data.site, pool)
    .await?;
    assert_eq!(vec![POST_BY_BOT, POST], names(&post_listings_show_hidden));

    // Make sure that hidden field is true.
    assert!(&post_listings_show_hidden.first().is_some_and(|p| p.hidden));

    cleanup(data, pool).await
  }

  #[tokio::test]
  #[serial]
  async fn post_listings_hide_nsfw() -> LemmyResult<()> {
    let pool = &build_db_pool()?;
    let pool = &mut pool.into();
    let data = init_data(pool).await?;

    // Mark a post as nsfw
    let update_form = PostUpdateForm {
      nsfw: Some(true),
      ..Default::default()
    };

    Post::update(pool, data.inserted_bot_post.id, &update_form).await?;

    // Make sure you don't see the nsfw post in the regular results
    let post_listings_hide_nsfw = data.default_post_query().list(&data.site, pool).await?;
    assert_eq!(vec![POST], names(&post_listings_hide_nsfw));

    // Make sure it does come back with the show_nsfw option
    let post_listings_show_nsfw = PostQuery {
      sort: Some(PostSortType::New),
      show_nsfw: Some(true),
      local_user: Some(&data.local_user_view.local_user),
      ..Default::default()
    }
    .list(&data.site, pool)
    .await?;
    assert_eq!(vec![POST_BY_BOT, POST], names(&post_listings_show_nsfw));

    // Make sure that nsfw field is true.
    assert!(&post_listings_show_nsfw.first().is_some_and(|p| p.post.nsfw));

    cleanup(data, pool).await
  }

  async fn cleanup(data: Data, pool: &mut DbPool<'_>) -> LemmyResult<()> {
    let num_deleted = Post::delete(pool, data.inserted_post.id).await?;
    Community::delete(pool, data.inserted_community.id).await?;
    Person::delete(pool, data.local_user_view.person.id).await?;
    Person::delete(pool, data.inserted_bot.id).await?;
    Person::delete(pool, data.blocked_local_user_view.person.id).await?;
    Instance::delete(pool, data.inserted_instance.id).await?;
    assert_eq!(1, num_deleted);

    Ok(())
  }

  async fn expected_post_view(data: &Data, pool: &mut DbPool<'_>) -> LemmyResult<PostView> {
    let (inserted_person, inserted_community, inserted_post) = (
      &data.local_user_view.person,
      &data.inserted_community,
      &data.inserted_post,
    );
    let agg = PostAggregates::read(pool, inserted_post.id).await?;

    Ok(PostView {
      post: Post {
        id: inserted_post.id,
        name: inserted_post.name.clone(),
        creator_id: inserted_person.id,
        url: None,
        body: None,
        alt_text: None,
        published: inserted_post.published,
        updated: None,
        community_id: inserted_community.id,
        removed: false,
        deleted: false,
        locked: false,
        nsfw: false,
        embed_title: None,
        embed_description: None,
        embed_video_url: None,
        thumbnail_url: None,
        ap_id: inserted_post.ap_id.clone(),
        local: true,
        language_id: LanguageId(47),
        featured_community: false,
        featured_local: false,
        url_content_type: None,
        scheduled_publish_time: None,
      },
      my_vote: None,
      unread_comments: 0,
      creator: Person {
        id: inserted_person.id,
        name: inserted_person.name.clone(),
        display_name: None,
        published: inserted_person.published,
        avatar: None,
        actor_id: inserted_person.actor_id.clone(),
        local: true,
        bot_account: false,
        banned: false,
        deleted: false,
        bio: None,
        banner: None,
        updated: None,
        inbox_url: inserted_person.inbox_url.clone(),
        matrix_user_id: None,
        ban_expires: None,
        instance_id: data.inserted_instance.id,
        private_key: inserted_person.private_key.clone(),
        public_key: inserted_person.public_key.clone(),
        last_refreshed_at: inserted_person.last_refreshed_at,
      },
      image_details: None,
      creator_banned_from_community: false,
      banned_from_community: false,
      creator_is_moderator: false,
      creator_is_admin: true,
      community: Community {
        id: inserted_community.id,
        name: inserted_community.name.clone(),
        icon: None,
        removed: false,
        deleted: false,
        nsfw: false,
        actor_id: inserted_community.actor_id.clone(),
        local: true,
        title: "nada".to_owned(),
        sidebar: None,
        description: None,
        updated: None,
        banner: None,
        hidden: false,
        posting_restricted_to_mods: false,
        published: inserted_community.published,
        instance_id: data.inserted_instance.id,
        private_key: inserted_community.private_key.clone(),
        public_key: inserted_community.public_key.clone(),
        last_refreshed_at: inserted_community.last_refreshed_at,
        followers_url: inserted_community.followers_url.clone(),
        inbox_url: inserted_community.inbox_url.clone(),
        moderators_url: inserted_community.moderators_url.clone(),
        featured_url: inserted_community.featured_url.clone(),
        visibility: CommunityVisibility::Public,
      },
      counts: PostAggregates {
        post_id: inserted_post.id,
        comments: 0,
        score: 0,
        upvotes: 0,
        downvotes: 0,
        published: agg.published,
        newest_comment_time_necro: inserted_post.published,
        newest_comment_time: inserted_post.published,
        featured_community: false,
        featured_local: false,
        hot_rank: RANK_DEFAULT,
        hot_rank_active: RANK_DEFAULT,
        controversy_rank: 0.0,
        scaled_rank: RANK_DEFAULT,
        community_id: inserted_post.community_id,
        creator_id: inserted_post.creator_id,
        instance_id: data.inserted_instance.id,
      },
      subscribed: SubscribedType::NotSubscribed,
      read: false,
      hidden: false,
      saved: false,
      creator_blocked: false,
    })
  }

  #[tokio::test]
  #[serial]
  async fn local_only_instance() -> LemmyResult<()> {
    let pool = &build_db_pool_for_tests();
    let pool = &mut pool.into();
    let data = init_data(pool).await?;

    Community::update(
      pool,
      data.inserted_community.id,
      &CommunityUpdateForm {
        visibility: Some(CommunityVisibility::LocalOnly),
        ..Default::default()
      },
    )
    .await?;

    let unauthenticated_query = PostQuery {
      ..Default::default()
    }
    .list(&data.site, pool)
    .await?;
    assert_eq!(0, unauthenticated_query.len());

    let authenticated_query = PostQuery {
      local_user: Some(&data.local_user_view.local_user),
      ..Default::default()
    }
    .list(&data.site, pool)
    .await?;
    assert_eq!(2, authenticated_query.len());

    let unauthenticated_post = PostView::read(pool, data.inserted_post.id, None, false).await;
    assert!(unauthenticated_post.is_err());

    let authenticated_post = PostView::read(
      pool,
      data.inserted_post.id,
      Some(&data.local_user_view.local_user),
      false,
    )
    .await;
    assert!(authenticated_post.is_ok());

    cleanup(data, pool).await?;
    Ok(())
  }

  #[tokio::test]
  #[serial]
  async fn post_listing_local_user_banned_from_community() -> LemmyResult<()> {
    let pool = &build_db_pool()?;
    let pool = &mut pool.into();
    let data = init_data(pool).await?;

    // Test that post view shows if local user is blocked from community
    let banned_from_comm_person = PersonInsertForm::test_form(data.inserted_instance.id, "jill");

    let inserted_banned_from_comm_person = Person::create(pool, &banned_from_comm_person).await?;

    let inserted_banned_from_comm_local_user = LocalUser::create(
      pool,
      &LocalUserInsertForm::test_form(inserted_banned_from_comm_person.id),
      vec![],
    )
    .await?;

    CommunityPersonBan::ban(
      pool,
      &CommunityPersonBanForm {
        community_id: data.inserted_community.id,
        person_id: inserted_banned_from_comm_person.id,
        expires: None,
      },
    )
    .await?;

    let post_view = PostView::read(
      pool,
      data.inserted_post.id,
      Some(&inserted_banned_from_comm_local_user),
      false,
    )
    .await?;

    assert!(post_view.banned_from_community);

    Person::delete(pool, inserted_banned_from_comm_person.id).await?;
    cleanup(data, pool).await
  }

  #[tokio::test]
  #[serial]
  async fn post_listing_local_user_not_banned_from_community() -> LemmyResult<()> {
    let pool = &build_db_pool()?;
    let pool = &mut pool.into();
    let data = init_data(pool).await?;

    let post_view = PostView::read(
      pool,
      data.inserted_post.id,
      Some(&data.local_user_view.local_user),
      false,
    )
    .await?;

    assert!(!post_view.banned_from_community);

    cleanup(data, pool).await
  }

  #[tokio::test]
  #[serial]
  async fn speed_check() -> LemmyResult<()> {
    let pool = &build_db_pool()?;
    let pool = &mut pool.into();
    let data = init_data(pool).await?;

    // Make sure the post_view query is less than this time
    let duration_max = Duration::from_millis(80);

    // Create some dummy posts
    let num_posts = 1000;
    for x in 1..num_posts {
      let name = format!("post_{x}");
      let url = Some(Url::parse(&format!("https://google.com/{name}"))?.into());

      let post_form = PostInsertForm {
        url,
        ..PostInsertForm::new(
          name,
          data.local_user_view.person.id,
          data.inserted_community.id,
        )
      };
      Post::create(pool, &post_form).await?;
    }

    // Manually trigger and wait for a statistics update to ensure consistent and high amount of
    // accuracy in the statistics used for query planning
    println!("🧮 updating database statistics");
    let conn = &mut get_conn(pool).await?;
    conn.batch_execute("ANALYZE;").await?;

    // Time how fast the query took
    let now = Instant::now();
    PostQuery {
      sort: Some(PostSortType::Active),
      local_user: Some(&data.local_user_view.local_user),
      ..Default::default()
    }
    .list(&data.site, pool)
    .await?;

    let elapsed = now.elapsed();
    println!("Elapsed: {:.0?}", elapsed);

    assert!(
      elapsed.lt(&duration_max),
      "Query took {:.0?}, longer than the max of {:.0?}",
      elapsed,
      duration_max
    );

    cleanup(data, pool).await
  }

  #[tokio::test]
  #[serial]
  async fn post_listings_no_comments_only() -> LemmyResult<()> {
    let pool = &build_db_pool()?;
    let pool = &mut pool.into();
    let data = init_data(pool).await?;

    // Create a comment for a post
    let comment_form = CommentInsertForm::new(
      data.local_user_view.person.id,
      data.inserted_post.id,
      "a comment".to_owned(),
    );
    Comment::create(pool, &comment_form, None).await?;

    // Make sure it doesnt come back with the no_comments option
    let post_listings_no_comments = PostQuery {
      sort: Some(PostSortType::New),
      no_comments_only: Some(true),
      local_user: Some(&data.local_user_view.local_user),
      ..Default::default()
    }
    .list(&data.site, pool)
    .await?;

    assert_eq!(vec![POST_BY_BOT], names(&post_listings_no_comments));

    cleanup(data, pool).await
  }

  #[tokio::test]
  #[serial]
  async fn post_listing_private_community() -> LemmyResult<()> {
    let pool = &build_db_pool()?;
    let pool = &mut pool.into();
    let mut data = init_data(pool).await?;

    // Mark community as private
    Community::update(
      pool,
      data.inserted_community.id,
      &CommunityUpdateForm {
        visibility: Some(CommunityVisibility::Private),
        ..Default::default()
      },
    )
    .await?;

    // No posts returned without auth
    let read_post_listing = PostQuery {
      community_id: Some(data.inserted_community.id),
      ..Default::default()
    }
    .list(&data.site, pool)
    .await?;
    assert_eq!(0, read_post_listing.len());
    let post_view = PostView::read(pool, data.inserted_post.id, None, false).await;
    assert!(post_view.is_err());

    // No posts returned for non-follower who is not admin
    data.local_user_view.local_user.admin = false;
    let read_post_listing = PostQuery {
      community_id: Some(data.inserted_community.id),
      local_user: Some(&data.local_user_view.local_user),
      ..Default::default()
    }
    .list(&data.site, pool)
    .await?;
    assert_eq!(0, read_post_listing.len());
    let post_view = PostView::read(
      pool,
      data.inserted_post.id,
      Some(&data.local_user_view.local_user),
      false,
    )
    .await;
    assert!(post_view.is_err());

    // Admin can view content without following
    data.local_user_view.local_user.admin = true;
    let read_post_listing = PostQuery {
      community_id: Some(data.inserted_community.id),
      local_user: Some(&data.local_user_view.local_user),
      ..Default::default()
    }
    .list(&data.site, pool)
    .await?;
    assert_eq!(2, read_post_listing.len());
    let post_view = PostView::read(
      pool,
      data.inserted_post.id,
      Some(&data.local_user_view.local_user),
      true,
    )
    .await;
    assert!(post_view.is_ok());
    data.local_user_view.local_user.admin = false;

    // User can view after following
    CommunityFollower::follow(
      pool,
      &CommunityFollowerForm {
        state: Some(CommunityFollowerState::Accepted),
        ..CommunityFollowerForm::new(data.inserted_community.id, data.local_user_view.person.id)
      },
    )
    .await?;
    let read_post_listing = PostQuery {
      community_id: Some(data.inserted_community.id),
      local_user: Some(&data.local_user_view.local_user),
      ..Default::default()
    }
    .list(&data.site, pool)
    .await?;
    assert_eq!(2, read_post_listing.len());
    let post_view = PostView::read(
      pool,
      data.inserted_post.id,
      Some(&data.local_user_view.local_user),
      true,
    )
    .await;
    assert!(post_view.is_ok());

    cleanup(data, pool).await
  }
}

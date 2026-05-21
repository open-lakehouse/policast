package com.policast.spark

/**
 * The identity of the user making the current query.
 * Resolved from Spark configuration properties at query time.
 */
case class QueryIdentity(
    role: String,
    region: Option[String],
    name: Option[String]
)
